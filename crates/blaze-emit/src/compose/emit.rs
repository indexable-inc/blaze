//! The composer's emission pass (WAVE-BACKEND.md piece K2; backend-next.md D97 point 2, D99, D100
//! points 3, 5, 6 and 11): ONE forward pass over the analysis pass's block order that turns a LIR
//! function into piece 0's [`Emitted`] record, every machine instruction copied from a template of
//! the [`Table`] and its marked fields filled. The encoders of enc.rs are never called here: what
//! the composer itself needs (a frame step, a move, a cell access, a constant piece, a jump, the
//! poll) is a template too, under the patterns of [`glue`].
//!
//! # Where values live
//!
//! - A value live across a block boundary has the HOME the analysis pass gave it (D100 point 5):
//!   a register for its whole interval, or its cell. The emission pass follows it: such a value
//!   is in its home whenever it is live, so each loop header sees it in the place the loop map
//!   says.
//! - A value defined and last used in one block (`Home::Local`) takes any free register here and
//!   may be evicted to its cell when none is free: the one whose next use in the block is
//!   farthest goes (a constant is not stored, it is made again). A register is free for a local
//!   value when no value of a register home holds it anywhere in the local's remaining interval
//!   (positions in the analysis pass's order; a register home live into a loop header is held to
//!   the loop's last back edge), so a local never meets a home.
//! - Callee-saved registers are taken only when the analysis pass saves them (`saved`): the
//!   prologue is made before the first instruction, so the set cannot grow during the pass.
//! - Every non-constant value has a 16-byte cell at `sp + offset`, above the stack slots (and a
//!   function with a block of two or more parameters has one more, the parallel copy's spare cell,
//!   at `sp + 0`, below the slots, so it is always in a cell load's reach). A cell or slot past
//!   the near forms' reach (32 KiB for a cell, 4 KiB for a slot's address) is reached by the far
//!   templates (`glue::FAR`: `add t, sp, #hi, lsl #12` and the access at `#lo`), up to 16 MiB. Like
//!   the Reference tier's cells, a register or a cell holds a value ZERO-EXTENDED TO 64 BITS (an
//!   `i1` is 0 or 1), so a template of width `w` gives a result already narrowed (T1's templates
//!   are enc.rs's sequences of the Reference tier's lowering).
//!
//! # Templates and patterns
//!
//! A LIR instruction's pattern is its op, its width (the result's bits; an `icmp`'s or `fcmp`'s
//! operands'; a `store`'s value's; 0 where none), its variant (a comparison's condition as its
//! `IntCond`/`FloatCond` index; a conversion's SOURCE width in bits; 0 otherwise), its result
//! form and its operand forms. An operand that is a constant inside the operand's immediate class
//! is an `Imm` operand (D100 point 6: `add`/`sub`/`ptr_add` and an `icmp` right side `U12`, `and`/
//! `or`/`xor` `Logical32`/`Logical64`, a shift amount below the width `Shift5`/`Shift6`, a load or
//! store offset `U12Scaled{k}` or `S9`); outside it, the constant is put in a register first.
//! Fused (one level, the inner value of one use made by the instruction just before): `br` of an
//! `icmp` (operands `[None, Block, a, b]`, inner `icmp` at operand 0, the width the comparison's)
//! and a `load`/`store` at offset 0 of a `ptr_add` of a non-constant offset (`[None, base, index,
//! None]` / `[value, None, base, index]`). No fused template in the table is an error, never the
//! unfused form chosen instead (rule 3).
//!
//! # Blocks, edges, loops and the poll
//!
//! An edge's arguments move into the target's parameters as one parallel copy, a cycle broken
//! through the spare cell (the moves form trees hanging off disjoint cycles, as each parameter
//! has one writer, so one cycle is broken at a time and one spare cell serves every edge). No
//! scratch register is held from one move to the next: a cell-to-cell move takes one for its two
//! templates, and a far cell's address is a template temporary. A conditional edge with
//! moves gets a stub after the fall-through path. Each back edge (the analysis pass's
//! `back_edges`) runs the poll after its moves (D100 point 3: `adrp x16, poll@PAGE; ldr xzr,
//! [x16, poll@PAGEOFF]`, a `Page21` and a `PageOff12 { scale: 3 }` relocation to the runtime's
//! poll page [`POLL_PAGE`], and a [`Poll`] row), so at the poll every value live into the header
//! is in its loop-map place. `loops[k].at` is header `k`'s first code byte, the k-th header in the
//! analysis pass's order being loop row `k`; a live row's cell or slot offset is filled here.
//!
//! # What this tier refuses by name (the Reference tier serves them)
//!
//! Values that are not an integer of at most 64 bits, a pointer, an `f32` or an `f64` (compounds
//! other than an overflow operation's or a `cmpxchg`'s pair, which lives in its cell:
//! `Composer::overflow`, `Composer::atomic`; `i128`, vectors); `sret`/`byval` parameters, more
//! arguments than registers, calls returning two values; landing blocks, `invoke`, `resume`;
//! atomics of an `i1` or a float, bulk memory, `asm`, target intrinsics, a thread-local's
//! `data_addr` (its `tls_addr` is served: `Composer::tls_addr`), tail calls, coroutines'
//! `suspend`; a frame of 16 MiB or more, or a slot aligned past 16 bytes; the function's own
//! data aligned past a page (as the Reference tier).
//!
//! # The function's own data
//!
//! A `data_addr` of an own row is an own-data site (`adrp; add` with `Page21` and `PageOff12 {
//! scale: 0 }` relocations to `OWN_CODE` or `OWN_RELRO` plus the row's offset), the rows placed
//! after the code as the Reference tier's record form places them (pass/sites.rs): a row
//! holding no address in the code part, one holding addresses in the relro part with a relro
//! word per address (`Composer::place_rows`).
//!
//! # Scratch registers (end-state.md D193 point 6)
//!
//! The description's scratch pair is a [`Pool`] (scratch.rs): a use that spans templates takes a
//! [`Scratch`] token and gives it back; a template's `TEMP0`/`TEMP1` are lent from the free
//! registers for that template alone; `Composer::place` checks every template's scratch inputs,
//! result and clobbers against the pool, and every instruction ends with the pool idle. A frame
//! past one page under the saved registers is probed by the `glue::PROBE` template (the
//! Reference tier's sequence, pass/probe.rs, with its count in a lent temporary).

use blaze_code::record::{OWN_CODE, OWN_RELRO, PositionField, PositionPart, PositionRow, SiteKind, Space, Symbol, Target, Word as RelroWord};
use blaze_code::unwind::{Action, Frame, INST, Row};
use blaze_lir::func::{BlockKind, DataId, Ext, ParamKind, RefId, RefKind, SigId};
use blaze_lir::op::{Field as LirField, Layout, Target as Edge, Word, walk};
use blaze_lir::{Block, Flags, IntCond, Op, Ordering, Ty, Value, View};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::table::{RESULT, TEMP0, TEMP1};
use blaze_tier::{
    Emitted, Field, FieldKind, Form, ImmClass, Inner, Live, Named, Names, OUTERMOST, Outside, Pattern, Place, Poll,
    Position, Reg, RegClass, RegDesc, RegSet, Reloc, RelocKind, Table, TableError, Template, TierError, TierName,
};

use super::analysis::{Analysis, Home};
use super::loop_map::slot_of;
use crate::scratch::{Pool, Scratch, ScratchError, Use};
use crate::pass::probe::PROBE_STEP;

/// The C name of the runtime's poll page: the launcher maps it readable, and makes it unreadable
/// to stop every running loop at its next back edge (D100 point 3; piece L's fault handler).
pub(crate) const POLL_PAGE: &str = "blaze_poll_page";

const NONE: u32 = u32::MAX;
/// A branch target that is a stub label, not a block.
const LABEL: u32 = 1 << 31;
/// A register's holder that is a temporary of the current instruction.
const TEMP: u32 = u32::MAX - 1;
const TIER: TierName = TierName::Fast;
/// The near cell loads' reach: `ldr x, [sp, #imm12 * 8]`; past it, the far templates.
const CELL_REACH: u32 = 4096 * 8;
/// The near slot address's reach: `add x, sp, #imm12`; past it, the far template.
const SLOT_REACH: u32 = 4096;
/// The frame `sub sp` can make in two steps (`lsl #12` and plain), and the far templates' reach
/// (`add t, sp, #imm12, lsl #12` and an offset under 4 KiB).
const FRAME_REACH: u32 = 1 << 24;

/// The composer's own patterns: what it emits besides LIR's instructions. Each is a row the
/// table generator writes like any other (T1); the arm64 instructions each stands for are given.
/// Operand numbers are piece 0's (inputs 0..4, [`RESULT`], [`TEMP0`], [`TEMP1`]).
pub(crate) mod glue {
    use blaze_code::record::SiteKind;
    use blaze_lir::{IntCond, Op};
    use blaze_tier::{Form, Inner, Pattern, RegClass};

    const N: Form = Form::None;

    /// `stp x29, x30, [sp, #-16]!; mov x29, sp`.
    pub(crate) const ENTER: u8 = 1;
    /// `stp a, b, [sp, #-16]!` (`x` or `d` registers): operands 0 and 1.
    pub(crate) const SAVE_PAIR: u8 = 2;
    /// `sub sp, sp, #imm` (operand 0, `U12`).
    pub(crate) const SUB_SP: u8 = 3;
    /// `sub sp, sp, #imm, lsl #12` (operand 0, `U12Lsl12`: the 12-bit field's value).
    pub(crate) const SUB_SP_HI: u8 = 4;
    /// `sub sp, x29, #imm` (operand 0, `U12`).
    pub(crate) const SP_FROM_FP: u8 = 5;
    /// `ldp a, b, [sp], #16`: operands 0 and 1.
    pub(crate) const RESTORE_PAIR: u8 = 6;
    /// `ldp x29, x30, [sp], #16`.
    pub(crate) const LEAVE: u8 = 7;
    /// The stack probe of a frame past one page (pass/probe.rs's sequence, its count in TEMP0):
    /// `movz t, #(F & 0xffff); movk t, #(F >> 16), lsl #16; loop: sub sp, sp, #1, lsl #12;
    /// str xzr, [sp]; sub t, t, #1, lsl #12; cmp t, #1, lsl #12; b.hs loop; sub sp, sp, t, uxtx`.
    /// Operand 0 is `F` (`Slot`: a `Frame { scale: 0 }` field of 24 bits), at least 4096.
    pub(crate) const PROBE: u8 = 8;

    /// The variant of a cell access or slot address past the near form's reach: `add TEMP0 (or
    /// the result), sp, #hi, lsl #12`, then the access at `#lo` from it.
    pub(crate) const FAR: u8 = 1;

    /// A `fence` that orders loads only (`dmb ishld`: an acquire fence).
    pub(crate) const FENCE_LOADS: u8 = 0;
    /// A `fence` that orders every access (`dmb ish`: release, acq_rel, seq_cst).
    pub(crate) const FENCE_ALL: u8 = 1;

    /// An atomic's variant: bit 0 when it releases, bit 1 when it acquires (pass/atomic.rs
    /// `sides`), bits 2.. an `atomic_rmw`'s kind (`AtomicRmw as u8`; 0 for the other atomics).
    pub(crate) const fn atomic(rmw: u8, acq: bool, rel: bool) -> u8 {
        (rmw << 2) | ((acq as u8) << 1) | rel as u8
    }

    /// A `tls_addr`'s first template: the thread's root address on `os` (`Os as u8`).
    pub(crate) const fn tls_root(os: blaze_tier::Os) -> u8 {
        os as u8
    }

    /// A `tls_addr`'s second template: the table lookup and, failing it, the getter's call.
    pub(crate) const TLS_LOOKUP: u8 = 2;

    const fn p(op: Op, width: u8, variant: u8, result: Form, operands: [Form; 4]) -> Pattern {
        Pattern { op, width, variant, result, operands, inner: None }
    }

    /// A frame step: `op` `stack_pointer` with no result (LIR's has one), `variant` the step.
    pub(crate) const fn frame(step: u8, operands: [Form; 4]) -> Pattern {
        p(Op::StackPointer, 64, step, N, operands)
    }

    /// `ldr x/d RESULT, [sp, #off]`: the load of a cell, X's shape (a `load` of a fused
    /// `slot_addr`; the `Frame { scale: 3 }` field is operand 2).
    pub(crate) const fn cell_load(class: RegClass) -> Pattern {
        Pattern {
            op: Op::Load,
            width: 64,
            variant: 0,
            result: Form::Reg(class),
            operands: [N, N, Form::Slot, N],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 0 }),
        }
    }

    /// `str x/d op0, [sp, #off]`: the store to a cell (the `Frame { scale: 3 }` field is
    /// operand 3).
    pub(crate) const fn cell_store(class: RegClass) -> Pattern {
        Pattern {
            op: Op::Store,
            width: 64,
            variant: 0,
            result: N,
            operands: [Form::Reg(class), N, N, Form::Slot],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 1 }),
        }
    }

    /// A cell's load past `CELL_REACH` (32 KiB): `add RESULT, sp, #hi, lsl #12; ldr xRESULT,
    /// [RESULT, #lo]` for a general register (no temporary), `add TEMP0, sp, ..; ldr dRESULT,
    /// [TEMP0, #lo]` for a float one. Operand 2 a `Frame { scale: 3 }` field of 21 bits, split.
    pub(crate) const fn cell_load_far(class: RegClass) -> Pattern {
        Pattern { variant: FAR, ..cell_load(class) }
    }

    /// A cell's store past `CELL_REACH` (32 KiB): `add TEMP0, sp, #hi, lsl #12; str op0, [TEMP0,
    /// #lo]`. Operand 3 a `Frame { scale: 3 }` field of 21 bits, split.
    pub(crate) const fn cell_store_far(class: RegClass) -> Pattern {
        Pattern { variant: FAR, ..cell_store(class) }
    }

    /// A slot's address past `SLOT_REACH` (4 KiB): `add RESULT, sp, #hi, lsl #12; add RESULT,
    /// RESULT, #lo`. Operand 0 a `Frame { scale: 0 }` field of 24 bits, split.
    pub(crate) const fn slot_address_far() -> Pattern {
        Pattern { variant: FAR, ..slot_address() }
    }

    /// `mov RESULT, op0` / `fmov dRESULT, dop0`: a block parameter's move.
    pub(crate) const fn mov(class: RegClass) -> Pattern {
        p(Op::Param, 64, 0, Form::Reg(class), [Form::Reg(class), N, N, N])
    }

    /// `fmov dRESULT, xop0` (`to_fpr`) or `fmov xRESULT, dop0`: 64 bits between the classes.
    pub(crate) const fn cross(to_fpr: bool) -> Pattern {
        match to_fpr {
            true => p(Op::Bitcast, 64, 64, Form::Reg(RegClass::Fpr), [Form::Reg(RegClass::Gpr), N, N, N]),
            false => p(Op::Bitcast, 64, 64, Form::Reg(RegClass::Gpr), [Form::Reg(RegClass::Fpr), N, N, N]),
        }
    }

    /// The low `width` bits of op0, zero-extended (`trunc` from 64 bits).
    pub(crate) const fn narrow(width: u8) -> Pattern {
        p(Op::Trunc, width, 64, Form::Reg(RegClass::Gpr), [Form::Reg(RegClass::Gpr), N, N, N])
    }

    /// op0's low `from` bits sign-extended to 32 (Apple's arm64 argument rule for a `signext`
    /// parameter).
    pub(crate) const fn sext32(from: u8) -> Pattern {
        p(Op::Sext, 32, from, Form::Reg(RegClass::Gpr), [Form::Reg(RegClass::Gpr), N, N, N])
    }

    /// `ret`.
    pub(crate) const fn ret() -> Pattern {
        p(Op::Return, 0, 0, N, [N, N, N, N])
    }

    /// `b op0`.
    pub(crate) const fn jump() -> Pattern {
        p(Op::Jump, 0, 0, N, [Form::Block, N, N, N])
    }

    /// `cbnz op0, op1` (T1's unfused `br`).
    pub(crate) const fn branch() -> Pattern {
        p(Op::Br, 1, 0, N, [Form::Reg(RegClass::Gpr), Form::Block, N, N])
    }

    /// `cmp op2, op3; b.cond op1`: `br` of a fused `icmp` of `width`-bit operands.
    pub(crate) const fn compare_branch(width: u8, cond: IntCond, rhs: Form) -> Pattern {
        Pattern {
            op: Op::Br,
            width,
            variant: 0,
            result: N,
            operands: [N, Form::Block, Form::Reg(RegClass::Gpr), rhs],
            inner: Some(Inner { op: Op::Icmp, variant: cond as u8, operand: 0 }),
        }
    }

    /// `adrp x16, #0; ldr xzr, [x16, #0]`: the poll, its two relocations the composer's.
    pub(crate) const fn poll() -> Pattern {
        p(Op::Load, 64, 0, N, [N, N, N, N])
    }

    /// `adrp x16, word@PAGE; ldr x16, [x16, word@PAGEOFF]; blr x16` (a `Site(Call)` field).
    pub(crate) const fn call() -> Pattern {
        p(Op::Call, 0, 0, N, [Form::Site(SiteKind::Call), N, N, N])
    }

    /// `blr op0`.
    pub(crate) const fn call_indirect() -> Pattern {
        p(Op::CallIndirect, 0, 0, N, [Form::Reg(RegClass::Gpr), N, N, N])
    }

    /// `adrp RESULT, word@PAGE; ldr RESULT, [RESULT, word@PAGEOFF]` (a `Site(Load)` field):
    /// the import word's value, for `func_addr` and `data_addr`.
    pub(crate) const fn address(op: Op) -> Pattern {
        p(op, 64, 0, Form::Reg(RegClass::Gpr), [Form::Site(SiteKind::Load), N, N, N])
    }

    /// `adrp RESULT, row@PAGE; add RESULT, RESULT, row@PAGEOFF` (a `Site(Relro)` field): the
    /// address of a row of the function's own data, in the record's code or relro part (T1's
    /// row "a byte of the record's relro part"; the composer's `Page21` and `PageOff12 { scale:
    /// 0 }` relocations name `OWN_CODE` or `OWN_RELRO` and the row's offset there).
    pub(crate) const fn own_address() -> Pattern {
        p(Op::DataAddr, 64, 0, Form::Reg(RegClass::Gpr), [Form::Site(SiteKind::Relro), N, N, N])
    }

    /// `add RESULT, sp, #off` (a `Frame { scale: 0 }` field).
    pub(crate) const fn slot_address() -> Pattern {
        p(Op::SlotAddr, 64, 0, Form::Reg(RegClass::Gpr), [Form::Slot, N, N, N])
    }
}

/// Where a local value is now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Reg(Reg),
    Cell,
    /// A constant not in a register: made again where it is used.
    Remat,
    /// Not defined yet, or past its last use.
    Dead,
}

/// What fills one template operand.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    None,
    Reg(Reg),
    Imm(u64),
    /// A block (its index) or a stub label (`LABEL | index`).
    To(u32),
    /// A frame offset from `sp` in bytes.
    Frame(u32),
    /// An index into the record's targets.
    Site(u32),
}

/// A copy of an analysis-pass home, matched by value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum H {
    Cell,
    Reg(Reg),
    Local,
}

/// A register home's interval in order positions.
#[derive(Clone, Copy)]
struct Span {
    reg: Reg,
    start: u32,
    end: u32,
    value: u32,
}

/// A branch field to fill once every block is placed.
#[derive(Clone, Copy)]
struct Patch {
    start: u32,
    len: u32,
    field: Field,
    to: u32,
    /// The bits the whole displacement may use: the most `from_bit + width` of its fields.
    reach: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Reg(Reg),
    Cell(u32),
    Const(u32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dst {
    Reg(Reg),
    Cell(u32),
}

#[derive(Clone, Copy)]
struct Move {
    src: Src,
    dst: Dst,
    class: RegClass,
}

/// Where an instruction's result is written, and what follows the template.
#[derive(Clone, Copy)]
enum Dest {
    /// Into its home register or its local register.
    Direct(Reg),
    /// Into a temporary (its home is an operand's register), then moved home.
    Move { temp: Reg, home: Reg },
    /// Into a temporary, then stored to its cell.
    Store(Reg),
    /// Into a temporary, then dropped (a local with no use).
    Drop(Reg),
}

impl Dest {
    fn reg(self) -> Reg {
        match self {
            Dest::Direct(r) | Dest::Store(r) | Dest::Drop(r) | Dest::Move { temp: r, .. } => r,
        }
    }
}

fn refused(inst: usize, what: &'static str) -> TierError {
    TierError::Refused { tier: TIER, inst: Some(inst as u32), what }
}

fn refused_fn(what: &'static str) -> TierError {
    TierError::Refused { tier: TIER, inst: None, what }
}

fn too_large(what: &'static str) -> TierError {
    TierError::TooLarge { tier: TIER, what }
}

fn bad_field(field: u32, what: &'static str) -> TierError {
    TierError::Table { tier: TIER, error: TableError::Field { row: u32::MAX, field, what } }
}

const fn inverse(cond: IntCond) -> IntCond {
    match cond {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Uge => IntCond::Ult,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sge => IntCond::Slt,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
    }
}

const fn signed(cond: IntCond) -> bool {
    matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge)
}

const fn mask(width: u32) -> u64 {
    match width {
        64.. => u64::MAX,
        w => (1u64 << w) - 1,
    }
}

/// The composer over one function. `'v` is the function's, `'r` the borrowed inputs', `'a` the
/// record's allocator's.
struct Composer<'v, 'r, 'a> {
    view: View<'v>,
    an: &'r Analysis<'r>,
    table: &'r dyn Table,
    regs: &'r RegDesc,
    names: &'r dyn Names,
    out: Emitted<'a>,
    /// The description's general scratch registers (scratch.rs): taken and given back, or lent
    /// to one template; idle between instructions.
    scratch: Pool,
    /// The parallel copy's spare cell (`sp + 0`), or `NONE` where no block has two parameters.
    spare: u32,
    /// The instruction being lowered (for errors).
    inst: usize,
    /// Per instruction: its position in the analysis pass's order, or `NONE`.
    pos: AVec<'a, u32>,
    /// Per value: its cell's offset from `sp`, or `NONE` (a constant, or no result).
    cell: AVec<'a, u32>,
    slot_at: AVec<'a, u32>,
    uses: AVec<'a, u32>,
    /// Per instruction: its first operand occurrence (`occ_first[inst]..occ_first[inst + 1]`).
    occ_first: AVec<'a, u32>,
    occ_val: AVec<'a, u32>,
    /// Per occurrence: the next instruction of the block using the same value, or `NONE`.
    occ_next: AVec<'a, u32>,
    /// Per value: its first use after its definition in its block, or `NONE`.
    def_next: AVec<'a, u32>,
    loc: AVec<'a, Loc>,
    in_cell: AVec<'a, bool>,
    next: AVec<'a, u32>,
    /// Per instruction: made by the next instruction's fused template.
    fused: AVec<'a, bool>,
    /// Per register number: the local value it holds, [`TEMP`], or `NONE`.
    holder: [u32; 64],
    /// Registers an operand or the result of the current instruction holds.
    pinned: RegSet,
    temps: RegSet,
    /// Register homes' intervals, sorted by register then start; `reg_first[r]..reg_first[r+1]`.
    spans: AVec<'a, Span>,
    reg_first: [u32; 65],
    /// Spilled around the current template: register homes to load again after it.
    reload: AVec<'a, (Reg, u32)>,
    block_at: AVec<'a, u32>,
    /// Per block: its loop row, or `NONE`.
    header_loop: AVec<'a, u32>,
    /// Per loop row: its last back edge's position.
    loop_end: AVec<'a, u32>,
    labels: AVec<'a, u32>,
    patches: AVec<'a, Patch>,
    moves: AVec<'a, Move>,
    poll_target: u32,
    /// The saved pairs, in the prologue's order.
    pairs: AVec<'a, (Reg, Reg)>,
    /// Per row of the function's own data: its index among the placed rows, or `NONE`.
    row_index: AVec<'a, u32>,
    /// The rows to place after the code, in the order first reached (a `data_addr`, or a word
    /// of a placed row).
    rows: AVec<'a, u32>,
    /// Per placed row, once placed: in the relro part (else the code), and its offset there.
    row_at: AVec<'a, (bool, u32)>,
    /// Each own-data site: its `Page21` relocation's index (the `PageOff12` follows) and the
    /// placed row it reaches, resolved when the rows are placed.
    own_sites: AVec<'a, (u32, u32)>,
    /// Each relro word naming another own row: (the word's offset, the placed row, the addend).
    inner: AVec<'a, (u32, u32, i64)>,
}

/// The emission pass: `view` as `an` says, every instruction from `table` (piece K2).
pub(crate) fn emit<'a>(
    view: View<'_>,
    an: &Analysis<'_>,
    table: &dyn Table,
    regs: &RegDesc,
    names: &dyn Names,
    alloc: Alloc<'a>,
) -> Result<Emitted<'a>, TierError> {
    let mut c = Composer::new(view, an, table, regs, names, alloc)?;
    c.prologue()?;
    c.entry_params()?;
    c.blocks()?;
    c.finish()?;
    Ok(c.out)
}

impl<'v, 'r, 'a> Composer<'v, 'r, 'a> {
    fn new(
        view: View<'v>,
        an: &'r Analysis<'r>,
        table: &'r dyn Table,
        regs: &'r RegDesc,
        names: &'r dyn Names,
        alloc: Alloc<'a>,
    ) -> Result<Composer<'v, 'r, 'a>, TierError> {
        if view.head.ptr_bits != 64 {
            return Err(refused_fn("a target whose pointers are not 64 bits"));
        }
        let n = view.len();
        if n >= (LABEL - 2) as usize || view.blocks() >= LABEL as usize {
            return Err(too_large("the function's instruction count"));
        }
        if an.order.first() != Some(&Block(0)) {
            return Err(refused_fn("an analysis order that does not start at the entry block"));
        }
        for b in 0..view.blocks() {
            if matches!(view.block_kind(Block(b as u32)), BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate) {
                return Err(refused_fn("a landing block (unwinding edges are the Reference tier's)"));
            }
        }
        let scratch = Pool::of(regs).map_err(|error| refused_fn(error.what()))?;
        let mut c = Composer {
            view,
            an,
            table,
            regs,
            names,
            out: Emitted::new_in(alloc),
            scratch,
            spare: NONE,
            inst: 0,
            pos: Vec::from_elem_in(NONE, n, alloc),
            cell: Vec::from_elem_in(NONE, n, alloc),
            slot_at: Vec::with_capacity_in(view.slots.len(), alloc),
            uses: Vec::from_elem_in(0, n, alloc),
            occ_first: Vec::from_elem_in(0, n + 1, alloc),
            occ_val: Vec::new_in(alloc),
            occ_next: Vec::new_in(alloc),
            def_next: Vec::from_elem_in(NONE, n, alloc),
            loc: Vec::from_elem_in(Loc::Dead, n, alloc),
            in_cell: Vec::from_elem_in(false, n, alloc),
            next: Vec::from_elem_in(NONE, n, alloc),
            fused: Vec::from_elem_in(false, n, alloc),
            holder: [NONE; 64],
            pinned: RegSet::EMPTY,
            temps: RegSet::EMPTY,
            spans: Vec::new_in(alloc),
            reg_first: [0; 65],
            reload: Vec::new_in(alloc),
            block_at: Vec::from_elem_in(NONE, view.blocks(), alloc),
            header_loop: Vec::from_elem_in(NONE, view.blocks(), alloc),
            loop_end: Vec::new_in(alloc),
            labels: Vec::new_in(alloc),
            patches: Vec::new_in(alloc),
            moves: Vec::new_in(alloc),
            poll_target: NONE,
            pairs: Vec::new_in(alloc),
            row_index: Vec::from_elem_in(NONE, view.datas.len(), alloc),
            rows: Vec::new_in(alloc),
            row_at: Vec::new_in(alloc),
            own_sites: Vec::new_in(alloc),
            inner: Vec::new_in(alloc),
        };
        c.positions()?;
        c.occurrences(alloc)?;
        c.frame()?;
        c.loops()?;
        c.homes()?;
        Ok(c)
    }

    fn home(&self, v: u32) -> H {
        match self.an.home[v as usize] {
            Home::Cell => H::Cell,
            Home::Reg(r) => H::Reg(r),
            Home::Local => H::Local,
        }
    }

    /// Each instruction's place in the analysis pass's order.
    fn positions(&mut self) -> Result<(), TierError> {
        let mut p: u32 = 0;
        for &b in self.an.order.iter() {
            if b.index() >= self.view.blocks() {
                return Err(refused_fn("an analysis order naming a block the function lacks"));
            }
            for inst in self.view.block(b) {
                if self.pos[inst] != NONE {
                    return Err(refused_fn("an analysis order naming a block twice"));
                }
                self.pos[inst] = p;
                p += 1;
            }
        }
        Ok(())
    }

    /// Every value operand of every placed instruction, its use count, and each occurrence's next
    /// use in its block (walked backward, block by block, no recursion).
    fn occurrences(&mut self, alloc: Alloc<'a>) -> Result<(), TierError> {
        let view = self.view;
        let n = view.len();
        let mut at: u32 = 0;
        for inst in 0..n {
            self.occ_first[inst] = at;
            if self.pos[inst] == NONE {
                continue;
            }
            let mut values = |v: u32| {
                self.occ_val.push(v);
                self.occ_next.push(NONE);
                at += 1;
            };
            match view.op(inst).info().layout {
                Layout::Data(LirField::Value) => values(view.data[inst]),
                Layout::Extra(fields) => {
                    let words = &view.extra[view.data[inst] as usize..];
                    walk(words, fields, Ty(view.tys[inst]), view.tys, &mut |word| -> Result<(), ()> {
                        if let Word::Value(v) = word {
                            values(v);
                        }
                        Ok(())
                    })
                    .map_err(|_| refused(inst, "operands that run past their words"))?;
                }
                Layout::None | Layout::Data(_) => {}
            }
        }
        self.occ_first[n] = at;
        for &v in self.occ_val.iter() {
            if v as usize >= n {
                return Err(refused_fn("an operand naming no instruction"));
            }
            self.uses[v as usize] += 1;
        }
        let mut seen: AVec<'a, u32> = Vec::from_elem_in(NONE, n, alloc);
        for &b in self.an.order.iter() {
            let range = view.block(b);
            for inst in range.clone().rev() {
                let (from, to) = (self.occ_first[inst] as usize, self.occ_first[inst + 1] as usize);
                for k in from..to {
                    self.occ_next[k] = seen[self.occ_val[k] as usize];
                }
                self.def_next[inst] = seen[inst];
                for k in from..to {
                    seen[self.occ_val[k] as usize] = inst as u32;
                }
            }
            for inst in range {
                let (from, to) = (self.occ_first[inst] as usize, self.occ_first[inst + 1] as usize);
                for k in from..to {
                    seen[self.occ_val[k] as usize] = NONE;
                }
            }
        }
        Ok(())
    }

    /// The spare cell at `sp + 0` where a block has two parameters or more, stack slots after
    /// it, then a 16-byte cell per non-constant value; refused at 16 MiB.
    fn frame(&mut self) -> Result<(), TierError> {
        let view = self.view;
        let mut off: u32 = 0;
        if (0..view.blocks()).any(|b| view.params(Block(b as u32)).len() >= 2) {
            self.spare = 0;
            off = 16;
        }
        for slot in view.slots {
            if slot.align > 4 {
                return Err(refused_fn("a stack slot aligned past 16 bytes"));
            }
            let align = 1u32 << slot.align;
            off = off.next_multiple_of(align);
            self.slot_at.push(off);
            off = off.checked_add(slot.size).filter(|&it| it < FRAME_REACH).ok_or(too_large("the frame (16 MiB)"))?;
        }
        off = off.next_multiple_of(16);
        for v in 0..view.len() {
            let constant = self.const_bits(v).is_some() && self.home(v as u32) != H::Cell;
            if self.pos[v] == NONE || view.ty(Value(v as u32)).is_none() || constant {
                continue;
            }
            if off + 16 >= FRAME_REACH {
                return Err(too_large("the frame (16 MiB)"));
            }
            self.cell[v] = off;
            off += 16;
        }
        self.out.frame = off.next_multiple_of(16);
        Ok(())
    }

    /// Loop rows matched to their headers (the k-th header in order is row k), each loop's last
    /// back edge, and the loop map copied with its offsets filled.
    fn loops(&mut self) -> Result<(), TierError> {
        let an = self.an;
        let mut k: u32 = 0;
        for &b in an.order.iter() {
            if an.back_edges.iter().any(|&(_, h)| h == b) {
                self.header_loop[b.index()] = k;
                k += 1;
            }
        }
        if k as usize != an.loops.len() {
            return Err(refused_fn("the analysis's loop rows do not match its back edges' headers"));
        }
        self.loop_end.resize(an.loops.len(), 0);
        for &(from, h) in an.back_edges.iter() {
            let row = self.header_loop[h.index()];
            let at = self.pos[self.view.terminator(from)];
            if at == NONE {
                return Err(refused_fn("a back edge from a block the order does not place"));
            }
            self.loop_end[row as usize] = self.loop_end[row as usize].max(at);
        }
        for &row in an.loops.iter() {
            self.out.loops.push(LoopHeader { at: 0, ..row });
        }
        for &live in an.live.iter() {
            let v = live.value as usize;
            let place = match live.place {
                Place::Reg(r) => Place::Reg(r),
                Place::Cell { .. } => match self.cell.get(v).copied().filter(|&it| it != NONE) {
                    Some(sp_offset) => Place::Cell { sp_offset },
                    None => return Err(refused_fn("a loop-map value with no cell")),
                },
                Place::Slot { .. } => match slot_of(self.view.len(), live.value).and_then(|slot| self.slot_at.get(slot).copied()) {
                    Some(sp_offset) => Place::Slot { sp_offset },
                    None => return Err(refused_fn("a loop-map slot the function lacks")),
                },
            };
            self.out.live.push(Live { value: live.value, place });
        }
        Ok(())
    }

    /// The register homes' intervals: [definition, last use], to the loop's last back edge when
    /// live into its header in a register.
    fn homes(&mut self) -> Result<(), TierError> {
        let an = self.an;
        let n = self.view.len();
        if an.home.len() < n || an.last_use.len() < n {
            return Err(refused_fn("an analysis shorter than the function"));
        }
        for v in 0..n {
            let H::Reg(reg) = self.home(v as u32) else { continue };
            if self.pos[v] == NONE {
                continue;
            }
            if !self.regs.allocatable.contains(reg) {
                return Err(refused_fn("a home register the description does not allocate"));
            }
            let start = self.pos[v];
            let last = an.last_use[v];
            let end = match last {
                NONE => start,
                last => self.pos.get(last as usize).copied().filter(|&it| it != NONE).unwrap_or(start).max(start),
            };
            self.spans.push(Span { reg, start, end, value: v as u32 });
        }
        for (row, header) in an.loops.iter().enumerate() {
            let from = header.first as usize;
            let to = from + header.count as usize;
            let Some(live) = an.live.get(from..to) else {
                return Err(refused_fn("a loop row past the analysis's live rows"));
            };
            for l in live {
                if let Place::Reg(_) = l.place {
                    let end = self.loop_end[row];
                    for s in self.spans.iter_mut().filter(|s| s.value == l.value) {
                        s.end = s.end.max(end);
                    }
                }
            }
        }
        self.spans.sort_by(|a, b| (a.reg, a.start, a.value).cmp(&(b.reg, b.start, b.value)));
        let mut at = 0usize;
        for r in 0..65usize {
            while at < self.spans.len() && (self.spans[at].reg.0 as usize) < r {
                at += 1;
            }
            self.reg_first[r] = at as u32;
        }
        Ok(())
    }

    fn spans_of(&self, r: Reg) -> &[Span] {
        let i = r.0 as usize & 63;
        &self.spans[self.reg_first[i] as usize..self.reg_first[i + 1] as usize]
    }

    /// Whether a register home holds `r` anywhere in `[from, to]`.
    fn reserved(&self, r: Reg, from: u32, to: u32) -> bool {
        self.spans_of(r).iter().take_while(|s| s.start <= to).any(|s| s.end >= from)
    }

    // ---- constants -------------------------------------------------------------------------

    /// A constant's bits, zero-extended from its width, if instruction `v` is one.
    fn const_bits(&self, v: usize) -> Option<u64> {
        let view = self.view;
        let ty = view.ty(Value(v as u32));
        let width = ty.bits(64);
        match view.op(v) {
            Op::Iconst => {
                let word = view.data[v];
                Some(match width {
                    ..=32 => u64::from(word) & mask(width),
                    _ => i64::from(word as i32) as u64 & mask(width),
                })
            }
            Op::IconstWide if width <= 64 => {
                let words = view.operands(v).constant();
                let low = u64::from(*words.first()?);
                let high = words.get(1).map_or(0, |&it| u64::from(it));
                Some((low | (high << 32)) & mask(width))
            }
            Op::Null | Op::Uninit if ty.is_scalar() && width <= 64 => Some(0),
            Op::Fconst if ty == Ty::F32 => Some(u64::from(view.data[v])),
            Op::FconstWide if ty == Ty::F64 => {
                let words = view.operands(v).constant();
                Some(u64::from(*words.first()?) | (u64::from(*words.get(1)?) << 32))
            }
            _ => None,
        }
    }

    /// `value` into `dst` by T1's constant templates: `iconst [Imm(Mov16)]` (one `movz`) below
    /// 65536, else `iconst [Imm(I64)]` (`movz` and three `movk`). An `Fpr` goes through a
    /// scratch register taken for the two templates.
    fn mov_imm(&mut self, dst: Reg, value: u64) -> Result<(), TierError> {
        let class = match value < 0x1_0000 {
            true => ImmClass::Mov16,
            false => ImmClass::I64,
        };
        let forms = [Form::Imm(class), Form::None, Form::None, Form::None];
        let pattern =
            Pattern { op: Op::Iconst, width: 64, variant: 0, result: Form::Reg(RegClass::Gpr), operands: forms, inner: None };
        let mut args = [Arg::None; 7];
        args[0] = Arg::Imm(value);
        match dst.class() {
            RegClass::Gpr => {
                args[RESULT as usize] = Arg::Reg(dst);
                self.put(pattern, &args).map(drop)
            }
            RegClass::Fpr => {
                let s = self.take_scratch()?;
                args[RESULT as usize] = Arg::Reg(s.reg());
                self.put(pattern, &args)?;
                self.mov(dst, s.reg())?;
                self.give_scratch(s)
            }
        }
    }

    fn materialize(&mut self, v: usize, dst: Reg) -> Result<(), TierError> {
        let bits = self.const_bits(v).ok_or(refused(self.inst, "a value made again that is not a constant"))?;
        self.mov_imm(dst, bits)
    }

    // ---- templates -------------------------------------------------------------------------

    /// The template of `pattern`, or the table's refusal naming it.
    fn find(&self, pattern: Pattern) -> Result<Template<'r>, TierError> {
        let table: &'r dyn Table = self.table;
        table.find(&pattern).ok_or(TierError::Table { tier: TIER, error: TableError::Missing { pattern } })
    }

    /// `pattern`'s template appended with its fields filled; its first byte's offset.
    fn put(&mut self, pattern: Pattern, args: &[Arg; 7]) -> Result<u32, TierError> {
        let t = self.find(pattern)?;
        self.place(&t, args)
    }

    /// The one place template bytes enter the code: the template's scratch use checked against
    /// the pool and its temporaries lent (scratch.rs, `Pool::lend`), then its bytes copied and
    /// every field filled.
    fn place(&mut self, t: &Template<'_>, args: &[Arg; 7]) -> Result<u32, TierError> {
        let mut inputs = RegSet::EMPTY;
        for arg in &args[..RESULT as usize] {
            if let Arg::Reg(r) = *arg {
                inputs = inputs.with(r);
            }
        }
        let result = match args[RESULT as usize] {
            Arg::Reg(r) => Some(r),
            _ => None,
        };
        let temps = match (t.fields.iter().any(|f| f.operand == TEMP0), t.fields.iter().any(|f| f.operand == TEMP1)) {
            (_, true) => 2,
            (true, false) => 1,
            (false, false) => 0,
        };
        let call = matches!(t.pattern.op, Op::Call | Op::CallIndirect);
        let lent = self.scratch.lend(Use { inputs, result, temps, clobbers: t.clobbers, call }).map_err(|error| self.scratch_refused(error))?;
        let start = self.out.code.len();
        let len = t.bytes.len();
        if start + len >= (u32::MAX / 2) as usize {
            return Err(too_large("the function's code (2 GiB)"));
        }
        let start = start as u32;
        self.out.code.extend_from_slice(t.bytes);
        let mut site_at = NONE;
        for (k, field) in t.fields.iter().enumerate() {
            let arg = match (field.operand, lent) {
                (TEMP0, [Some(r), _]) => Arg::Reg(r),
                (TEMP1, [_, Some(r)]) => Arg::Reg(r),
                (o, _) if o <= RESULT => args[o as usize],
                _ => return Err(bad_field(k as u32, "a field naming no operand")),
            };
            match (field.kind, arg) {
                (FieldKind::Reg, Arg::Reg(r)) => self.set_bits(start, len, field, u64::from(r.hw()))?,
                (FieldKind::Imm, Arg::Imm(value)) => self.set_bits(start, len, field, value)?,
                (FieldKind::Site(kind), Arg::Site(target)) => {
                    let at = start + (u32::from(field.at) / 32) * INST;
                    if at == site_at {
                        continue;
                    }
                    site_at = at;
                    if kind == SiteKind::Relro {
                        // An own-data site: `target` is the placed row's index; the part and the
                        // row's offset there are filled when the rows are placed (`place_rows`).
                        self.own_sites.push((self.out.relocs.len() as u32, target));
                        self.out.relocs.push(Reloc { at, in_relro: false, kind: RelocKind::Page21, target: OWN_CODE, addend: 0 });
                        let off = RelocKind::PageOff12 { scale: 0 };
                        self.out.relocs.push(Reloc { at: at + INST, in_relro: false, kind: off, target: OWN_CODE, addend: 0 });
                        continue;
                    }
                    self.out.relocs.push(Reloc { at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                    self.out.relocs.push(Reloc { at: at + INST, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                }
                (FieldKind::Branch { .. }, Arg::To(to)) => {
                    let reach = t
                        .fields
                        .iter()
                        .filter(|f| f.operand == field.operand && matches!(f.kind, FieldKind::Branch { .. }))
                        .map(|f| u32::from(f.from_bit) + u32::from(f.width))
                        .max()
                        .unwrap_or(0);
                    self.patches.push(Patch { start, len: len as u32, field: *field, to, reach });
                }
                (FieldKind::Frame { scale }, Arg::Frame(bytes)) => {
                    if bytes & ((1u32 << scale) - 1) != 0 {
                        return Err(bad_field(k as u32, "a frame offset not a multiple of its field's unit"));
                    }
                    let units = u64::from(bytes >> scale);
                    // A far template splits one offset over two fields (`from_bit` 0 and 9, say):
                    // the reach is the operand's highest field bit, not this field's.
                    let reach = t
                        .fields
                        .iter()
                        .filter(|f| f.operand == field.operand && matches!(f.kind, FieldKind::Frame { .. }))
                        .map(|f| u32::from(f.from_bit) + u32::from(f.width))
                        .max()
                        .unwrap_or(0);
                    if reach < 64 && units >> reach != 0 {
                        return Err(too_large("a frame offset past its field"));
                    }
                    self.set_bits(start, len, field, units)?;
                }
                _ => return Err(bad_field(k as u32, "a field whose kind is not its operand's form")),
            }
        }
        Ok(start)
    }

    /// The pool's refusal as the tier's, naming the instruction.
    fn scratch_refused(&self, error: ScratchError) -> TierError {
        refused(self.inst, error.what())
    }

    /// A scratch register held over several templates (scratch.rs).
    fn take_scratch(&mut self) -> Result<Scratch, TierError> {
        self.scratch.take().map_err(|error| self.scratch_refused(error))
    }

    fn give_scratch(&mut self, s: Scratch) -> Result<(), TierError> {
        self.scratch.give(s).map_err(|error| self.scratch_refused(error))
    }

    /// Bits `[from_bit, from_bit + width)` of `value` into the template at `start`, bit `at` on.
    fn set_bits(&mut self, start: u32, len: usize, field: &Field, value: u64) -> Result<(), TierError> {
        let at = usize::from(field.at);
        let width = usize::from(field.width);
        if at + width > len * 8 {
            return Err(bad_field(u32::from(field.at), "a field past its template's bits"));
        }
        for k in 0..width {
            let from = usize::from(field.from_bit) + k;
            let bit = if from < 64 { (value >> from) & 1 } else { 0 };
            let i = at + k;
            let byte = start as usize + i / 8;
            let m = 1u8 << (i % 8);
            match bit {
                1 => self.out.code[byte] |= m,
                _ => self.out.code[byte] &= !m,
            }
        }
        Ok(())
    }

    /// `pattern` emitted with every live value its template or a call clobbers kept: locals
    /// evicted, register homes stored before and loaded again after. Its start and length.
    fn emit_op(&mut self, pattern: Pattern, args: &[Arg; 7], call: bool) -> Result<(u32, u32), TierError> {
        let t = self.find(pattern)?;
        let mut clobbers = t.clobbers;
        if call {
            clobbers = clobbers.union(self.regs.allocatable.minus(self.regs.callee_saved));
        }
        let hit = clobbers.0 & self.regs.allocatable.0;
        if hit != 0 && !call && hit & self.pinned.0 != 0 {
            return Err(refused(self.inst, "a template that clobbers its own operand's register"));
        }
        self.reload.clear();
        if hit != 0 {
            self.spill(RegSet(hit))?;
        }
        let start = self.place(&t, args)?;
        let len = t.bytes.len() as u32;
        let mut k = 0;
        while k < self.reload.len() {
            let (r, v) = self.reload[k];
            match self.const_bits(v as usize) {
                Some(_) => self.materialize(v as usize, r)?,
                None => self.load_cell(r, self.cell[v as usize])?,
            }
            k += 1;
        }
        self.reload.clear();
        Ok((start, len))
    }

    /// Keeps the values live after the current instruction that `set`'s registers hold.
    fn spill(&mut self, set: RegSet) -> Result<(), TierError> {
        let p = self.pos[self.inst];
        let mut bits = set.0;
        while bits != 0 {
            let r = Reg(bits.trailing_zeros() as u8);
            bits &= bits - 1;
            let held = self.holder[r.0 as usize];
            if held != NONE && held != TEMP {
                match self.next[held as usize] {
                    NONE => {
                        self.holder[r.0 as usize] = NONE;
                        self.loc[held as usize] = Loc::Dead;
                    }
                    _ => self.evict(r)?,
                }
            }
            let live = self.spans_of(r).iter().find(|s| s.start < p && s.end > p).map(|s| s.value);
            if let Some(v) = live {
                if self.const_bits(v as usize).is_none() {
                    self.store_cell(r, self.cell[v as usize])?;
                }
                self.reload.push((r, v));
            }
        }
        Ok(())
    }

    fn load_cell(&mut self, r: Reg, off: u32) -> Result<(), TierError> {
        if off == NONE {
            return Err(refused(self.inst, "a value with no cell read from its cell"));
        }
        let mut args = [Arg::None; 7];
        args[2] = Arg::Frame(off);
        args[RESULT as usize] = Arg::Reg(r);
        let pattern = match off < CELL_REACH {
            true => glue::cell_load(r.class()),
            false => glue::cell_load_far(r.class()),
        };
        self.put(pattern, &args).map(drop)
    }

    fn store_cell(&mut self, r: Reg, off: u32) -> Result<(), TierError> {
        if off == NONE {
            return Err(refused(self.inst, "a value with no cell stored to its cell"));
        }
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(r);
        args[3] = Arg::Frame(off);
        // Past the near reach the template's address is its TEMP0, lent from the free scratch
        // registers by `place`: never `r`, never a register the composer holds (D193 point 6).
        let pattern = match off < CELL_REACH {
            true => glue::cell_store(r.class()),
            false => glue::cell_store_far(r.class()),
        };
        self.put(pattern, &args).map(drop)
    }

    /// `dst = src`, within or across the classes.
    fn mov(&mut self, dst: Reg, src: Reg) -> Result<(), TierError> {
        if dst == src {
            return Ok(());
        }
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(src);
        args[RESULT as usize] = Arg::Reg(dst);
        let pattern = match (src.class(), dst.class()) {
            (a, b) if a == b => glue::mov(a),
            (_, RegClass::Fpr) => glue::cross(true),
            (_, RegClass::Gpr) => glue::cross(false),
        };
        self.put(pattern, &args).map(drop)
    }

    // ---- registers -------------------------------------------------------------------------

    /// A register of `class` for a value live over `[from, to]`: a free one, else the local
    /// whose next use is farthest evicted.
    fn take(&mut self, class: RegClass, from: u32, to: u32) -> Result<Reg, TierError> {
        let usable = self
            .regs
            .allocatable
            .of(class)
            .minus(self.regs.callee_saved.minus(self.an.saved))
            .minus(self.pinned);
        let mut bits = usable.0;
        while bits != 0 {
            let r = Reg(bits.trailing_zeros() as u8);
            bits &= bits - 1;
            if self.holder[r.0 as usize] == NONE && !self.reserved(r, from, to) {
                return Ok(r);
            }
        }
        let mut best: Option<(u32, Reg)> = None;
        let mut bits = usable.0;
        while bits != 0 {
            let r = Reg(bits.trailing_zeros() as u8);
            bits &= bits - 1;
            let held = self.holder[r.0 as usize];
            if held == NONE || held == TEMP || self.reserved(r, from, to) {
                continue;
            }
            let next = self.next[held as usize];
            if best.is_none_or(|(b, _)| next > b) {
                best = Some((next, r));
            }
        }
        match best {
            Some((_, r)) => {
                self.evict(r)?;
                Ok(r)
            }
            None => Err(refused(
                self.inst,
                "no register of the class is free: each holds a value live across blocks or an operand of this instruction",
            )),
        }
    }

    /// The local in `r` to its cell (a constant is only forgotten).
    fn evict(&mut self, r: Reg) -> Result<(), TierError> {
        let v = self.holder[r.0 as usize];
        self.holder[r.0 as usize] = NONE;
        if v == NONE || v == TEMP || self.loc[v as usize] != Loc::Reg(r) {
            return Ok(());
        }
        if self.const_bits(v as usize).is_some() {
            self.loc[v as usize] = Loc::Remat;
            return Ok(());
        }
        if !self.in_cell[v as usize] {
            self.store_cell(r, self.cell[v as usize])?;
            self.in_cell[v as usize] = true;
        }
        self.loc[v as usize] = Loc::Cell;
        Ok(())
    }

    fn temp(&mut self, class: RegClass) -> Result<Reg, TierError> {
        let p = self.pos[self.inst];
        let r = self.take(class, p, p)?;
        self.holder[r.0 as usize] = TEMP;
        self.temps = self.temps.with(r);
        self.pinned = self.pinned.with(r);
        Ok(r)
    }

    /// A loaded or stored value's register class and width: [`Composer::class_of`]'s, an `i1`
    /// refused (memory holds whole bytes: the Reference tier's rule, pass.rs, and the evaluator's).
    fn access_width(&self, v: Value) -> Result<(RegClass, u8), TierError> {
        match self.class_of(v)? {
            (RegClass::Gpr, 1) => Err(refused(self.inst, "an access of an `i1` (memory holds whole bytes)")),
            it => Ok(it),
        }
    }

    /// A value's register class, or the refusal of its type.
    fn class_of(&self, v: Value) -> Result<(RegClass, u8), TierError> {
        let ty = self.view.ty(v);
        let bits = ty.bits(64);
        if ty.is_scalar() && !ty.is_none() {
            if ty.is_float() && (bits == 32 || bits == 64) {
                return Ok((RegClass::Fpr, bits as u8));
            }
            if (ty.is_int() || ty.is_ptr()) && (1..=64).contains(&bits) {
                return Ok((RegClass::Gpr, bits as u8));
            }
        }
        Err(refused(
            self.inst,
            "a value that is not an integer of at most 64 bits, a pointer, an `f32` or an `f64` (the Reference tier's)",
        ))
    }

    /// Where `v` is now, for a move.
    fn src_of(&self, v: Value) -> Result<Src, TierError> {
        let i = v.index();
        Ok(match self.home(v.0) {
            H::Reg(r) => Src::Reg(r),
            H::Cell => Src::Cell(self.cell[i]),
            H::Local => match self.loc[i] {
                Loc::Reg(r) => Src::Reg(r),
                Loc::Cell => Src::Cell(self.cell[i]),
                Loc::Remat => Src::Const(v.0),
                Loc::Dead => return Err(refused(self.inst, "a value used where it is not defined")),
            },
        })
    }

    /// `v` in a register for the current instruction: its home, its local register, or a
    /// register it is loaded or made into. Pinned until the instruction ends.
    fn operand(&mut self, v: Value) -> Result<Reg, TierError> {
        let (class, _) = self.class_of(v)?;
        let i = v.index();
        let r = match self.home(v.0) {
            H::Reg(r) => r,
            H::Cell => {
                let r = self.temp(class)?;
                self.load_cell(r, self.cell[i])?;
                r
            }
            H::Local => match self.loc[i] {
                Loc::Reg(r) => r,
                Loc::Cell | Loc::Remat => {
                    let p = self.pos[self.inst];
                    let last = self.an.last_use[i];
                    let to = match last {
                        NONE => p,
                        last => self.pos.get(last as usize).copied().filter(|&it| it != NONE).unwrap_or(p).max(p),
                    };
                    let r = self.take(class, p, to)?;
                    match self.loc[i] {
                        Loc::Remat => self.materialize(i, r)?,
                        _ => self.load_cell(r, self.cell[i])?,
                    }
                    self.holder[r.0 as usize] = v.0;
                    self.loc[i] = Loc::Reg(r);
                    r
                }
                Loc::Dead => return Err(refused(self.inst, "a value used where it is not defined")),
            },
        };
        self.pinned = self.pinned.with(r);
        Ok(r)
    }

    /// `v` into `dst` (an argument, return or scratch register no value holds).
    fn value_into(&mut self, v: Value, dst: Reg) -> Result<(), TierError> {
        match self.src_of(v)? {
            Src::Reg(r) => self.mov(dst, r),
            Src::Cell(off) => self.load_cell(dst, off),
            Src::Const(c) => self.materialize(c as usize, dst),
        }
    }

    /// Where the current instruction's result is written.
    fn dest(&mut self, class: RegClass) -> Result<Dest, TierError> {
        let v = self.inst;
        let p = self.pos[v];
        Ok(match self.home(v as u32) {
            H::Reg(r) => {
                if self.pinned.contains(r) {
                    let temp = self.temp(class)?;
                    Dest::Move { temp, home: r }
                } else {
                    if self.holder[r.0 as usize] != NONE {
                        self.evict(r)?;
                    }
                    self.pinned = self.pinned.with(r);
                    Dest::Direct(r)
                }
            }
            H::Cell => Dest::Store(self.temp(class)?),
            H::Local if self.uses[v] == 0 => Dest::Drop(self.temp(class)?),
            H::Local => {
                let last = self.an.last_use[v];
                let to = match last {
                    NONE => p,
                    last => self.pos.get(last as usize).copied().filter(|&it| it != NONE).unwrap_or(p).max(p),
                };
                let r = self.take(class, p, to)?;
                self.holder[r.0 as usize] = v as u32;
                self.pinned = self.pinned.with(r);
                self.loc[v] = Loc::Reg(r);
                self.in_cell[v] = false;
                self.next[v] = self.def_next[v];
                Dest::Direct(r)
            }
        })
    }

    fn finish_dest(&mut self, dest: Dest) -> Result<(), TierError> {
        let v = self.inst;
        match dest {
            Dest::Direct(_) | Dest::Drop(_) => {}
            Dest::Move { temp, home } => self.mov(home, temp)?,
            Dest::Store(temp) => {
                self.store_cell(temp, self.cell[v])?;
                self.in_cell[v] = true;
                self.loc[v] = Loc::Cell;
            }
        }
        if let H::Reg(r) = self.home(v as u32) {
            self.loc[v] = Loc::Reg(r);
        }
        Ok(())
    }

    /// The bookkeeping after instruction `inst` (and its fused inner one): locals past their
    /// last use freed, temporaries freed, nothing pinned.
    fn after(&mut self, inst: usize) {
        let first = match inst > 0 && self.fused[inst - 1] {
            true => inst - 1,
            false => inst,
        };
        for at in first..=inst {
            let (from, to) = (self.occ_first[at] as usize, self.occ_first[at + 1] as usize);
            for k in from..to {
                let v = self.occ_val[k] as usize;
                if self.home(v as u32) != H::Local {
                    continue;
                }
                let next = self.occ_next[k];
                if next != NONE && next as usize <= inst {
                    continue;
                }
                match (self.loc[v], next) {
                    (Loc::Reg(r), NONE) => {
                        if self.holder[r.0 as usize] == v as u32 {
                            self.holder[r.0 as usize] = NONE;
                        }
                        self.loc[v] = Loc::Dead;
                    }
                    (_, NONE) => self.loc[v] = Loc::Dead,
                    (_, next) => self.next[v] = next,
                }
            }
        }
        let mut bits = self.temps.0;
        while bits != 0 {
            let r = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            self.holder[r] = NONE;
        }
        self.temps = RegSet::EMPTY;
        self.pinned = RegSet::EMPTY;
    }

    // ---- prologue, entry, blocks -----------------------------------------------------------

    fn prologue(&mut self) -> Result<(), TierError> {
        let none = [Arg::None; 7];
        self.put(glue::frame(glue::ENTER, [Form::None; 4]), &none)?;
        let saved = self.an.saved;
        let mut covered = RegSet::EMPTY;
        let (mut x_pairs, mut d_pairs) = (0u8, 0u8);
        for &(a, b) in self.regs.save_pairs {
            if !saved.contains(a) && !saved.contains(b) {
                continue;
            }
            covered = covered.with(a).with(b);
            let bit = match a.class() {
                RegClass::Gpr => a.0.checked_sub(19).map(|it| it / 2).filter(|&it| it < 5),
                RegClass::Fpr => a.0.checked_sub(40).map(|it| it / 2).filter(|&it| it < 4),
            };
            match (a.class(), bit) {
                (RegClass::Gpr, Some(bit)) => x_pairs |= 1 << bit,
                (RegClass::Fpr, Some(bit)) => d_pairs |= 1 << bit,
                _ => return Err(refused_fn("a saved pair the frame word cannot describe")),
            }
            // The platform's one order (blaze-code unwind.rs, "The frame word"): the higher
            // register at the lower address, `stp x20, x19`, so x19 sits at CFA - 24 - 16k.
            let form = Form::Reg(a.class());
            let mut args = none;
            args[0] = Arg::Reg(b);
            args[1] = Arg::Reg(a);
            self.put(glue::frame(glue::SAVE_PAIR, [form, form, Form::None, Form::None]), &args)?;
            self.pairs.push((b, a));
        }
        if !saved.minus(covered).is_empty() {
            return Err(refused_fn("a saved register in no pair of the description's"));
        }
        let frame = self.out.frame;
        let (hi, lo) = (frame >> 12, frame & 0xfff);
        // A guard page must not be stepped over (pass/probe.rs): where `sp` would drop more
        // than a page under the last saved pair before anything writes again, the probe walks
        // it down a page at a time, touching each (a frame here is a multiple of 16, so past
        // the bound it is at least 4096: the probe's loop runs at least once). No slot is
        // aligned past 16 (`frame`), so there is no rounding to count.
        if 16 + u64::from(frame) > u64::from(PROBE_STEP) {
            let mut args = none;
            args[0] = Arg::Frame(frame);
            self.put(glue::frame(glue::PROBE, [Form::Slot, Form::None, Form::None, Form::None]), &args)?;
        } else {
            if hi != 0 {
                let mut args = none;
                args[0] = Arg::Imm(u64::from(hi));
                self.put(glue::frame(glue::SUB_SP_HI, [Form::Imm(ImmClass::U12Lsl12), Form::None, Form::None, Form::None]), &args)?;
            }
            if lo != 0 {
                let mut args = none;
                args[0] = Arg::Imm(u64::from(lo));
                self.put(glue::frame(glue::SUB_SP, [Form::Imm(ImmClass::U12), Form::None, Form::None, Form::None]), &args)?;
            }
        }
        self.out.frame = frame + 16 * self.pairs.len() as u32;
        self.out.unwind_frame = Some(Frame { x_pairs, d_pairs });
        Ok(())
    }

    /// The entry block's parameters from the C convention's registers, zero-extended at once.
    fn entry_params(&mut self) -> Result<(), TierError> {
        let view = self.view;
        if view.head.sig as usize >= view.sigs.len() {
            return Err(refused_fn("no signature row"));
        }
        if view.sigs[view.head.sig as usize].bits & blaze_lir::func::SigRec::VARIADIC != 0 {
            return Err(refused_fn(crate::pass::variadic::VARIADIC_DEFINITION));
        }
        let sig = view.sig(SigId(view.head.sig));
        let params = view.params(Block(0));
        if params.len() != sig.params.len() {
            return Err(refused_fn("entry parameters that are not the signature's"));
        }
        let (mut g, mut f) = (0usize, 0usize);
        for (inst, param) in params.zip(sig.params.iter()) {
            self.inst = inst;
            if param.kind != ParamKind::Value as u8 {
                return Err(refused(inst, "an `sret` or `byval` parameter (the Reference tier's)"));
            }
            let (class, width) = self.class_of(Value(inst as u32))?;
            let src = match class {
                RegClass::Gpr => self.regs.args_gpr.get(g).copied().inspect(|_| g += 1),
                RegClass::Fpr => self.regs.args_fpr.get(f).copied().inspect(|_| f += 1),
            };
            let src = src.ok_or(refused(inst, "an entry parameter past the registers"))?;
            let narrow = class == RegClass::Gpr && width < 64;
            match self.home(inst as u32) {
                H::Reg(r) => {
                    match narrow {
                        true => self.narrow(r, src, width)?,
                        false => self.mov(r, src)?,
                    }
                    self.loc[inst] = Loc::Reg(r);
                }
                H::Cell | H::Local => {
                    match narrow {
                        true => {
                            let s = self.take_scratch()?;
                            self.narrow(s.reg(), src, width)?;
                            self.store_cell(s.reg(), self.cell[inst])?;
                            self.give_scratch(s)?;
                        }
                        false => self.store_cell(src, self.cell[inst])?,
                    }
                    self.in_cell[inst] = true;
                    self.loc[inst] = Loc::Cell;
                    self.next[inst] = self.def_next[inst];
                }
            }
            self.scratch_idle()?;
        }
        Ok(())
    }

    fn narrow(&mut self, dst: Reg, src: Reg, width: u8) -> Result<(), TierError> {
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(src);
        args[RESULT as usize] = Arg::Reg(dst);
        self.put(glue::narrow(width), &args).map(drop)
    }

    fn blocks(&mut self) -> Result<(), TierError> {
        let order = &self.an.order;
        for at in 0..order.len() {
            let b = order[at];
            let next = order.get(at + 1).copied();
            let here = self.out.code.len() as u32;
            self.block_at[b.index()] = here;
            let row = self.header_loop[b.index()];
            if row != NONE {
                self.out.loops[row as usize].at = here;
            }
            self.holder = [NONE; 64];
            let params = self.view.params(b);
            if b != Block(0) {
                for inst in params.clone() {
                    self.loc[inst] = match self.home(inst as u32) {
                        H::Reg(r) => Loc::Reg(r),
                        H::Cell | H::Local => Loc::Cell,
                    };
                    self.in_cell[inst] = true;
                    self.next[inst] = self.def_next[inst];
                }
            }
            for inst in params.end..self.view.block(b).end {
                self.lower(inst, b, next)?;
            }
        }
        Ok(())
    }

    /// The position map: a row where the span changes.
    fn mark(&mut self, inst: usize) {
        let at = self.out.code.len() as u32;
        let (lo, hi) = match self.view.loc(inst) {
            Some(span) => (span.index, span.index),
            None => (NONE, NONE),
        };
        if let Some(last) = self.out.positions.last_mut() {
            if last.span_lo == lo && last.span_hi == hi {
                return;
            }
            if last.at == at {
                last.span_lo = lo;
                last.span_hi = hi;
                return;
            }
        }
        self.out.positions.push(Position { at, frame: OUTERMOST, span_lo: lo, span_hi: hi });
    }

    // ---- instructions ----------------------------------------------------------------------

    /// Whether `inst` is made by the next instruction's fused template (D100 point 6).
    fn fuses_into_next(&self, inst: usize, block: Block) -> bool {
        let view = self.view;
        let next = inst + 1;
        if next >= view.block(block).end || self.home(inst as u32) != H::Local || self.uses[inst] != 1 {
            return false;
        }
        match (view.op(inst), view.op(next)) {
            (Op::Icmp, Op::Br) => {
                let mut o = view.operands(next);
                let cond = o.value();
                let mut i = view.operands(inst);
                let _ = i.int_cond();
                let a = i.value();
                cond.index() == inst && view.ty(a).bits(64) <= 64 && !view.ty(a).is_float()
            }
            (Op::PtrAdd, Op::Load | Op::Store) => {
                let mut o = view.operands(next);
                let ptr = match view.op(next) {
                    Op::Load => o.value(),
                    _ => {
                        let _ = o.value();
                        o.value()
                    }
                };
                let off = o.offset();
                let mut i = view.operands(inst);
                let _ = i.value();
                let index = i.value();
                // A 64-bit index only: the fused `[rn, rm]` adds the register as it is, where a
                // narrower `ptr_add` offset is sign-extended first (its own template's).
                ptr.index() == inst && off == 0 && view.ty(index).bits(64) == 64 && self.const_bits(index.index()).is_none()
            }
            _ => false,
        }
    }

    fn lower(&mut self, inst: usize, block: Block, next: Option<Block>) -> Result<(), TierError> {
        self.inst = inst;
        if self.fuses_into_next(inst, block) {
            self.fused[inst] = true;
            return Ok(());
        }
        self.mark(inst);
        let op = self.view.op(inst);
        match op {
            Op::Iconst | Op::IconstWide | Op::Null | Op::Uninit | Op::Fconst | Op::FconstWide => self.constant(inst)?,
            Op::SlotLive | Op::SlotDead | Op::Assume => {}
            Op::SlotAddr => self.slot_addr(inst)?,
            Op::FuncAddr | Op::DataAddr => self.address(inst, op)?,
            Op::Load => self.load(inst)?,
            Op::Store => self.store(inst)?,
            Op::Icmp => self.icmp(inst)?,
            Op::Fcmp => self.fcmp(inst)?,
            Op::Call | Op::CallIndirect => self.call(inst, op)?,
            Op::Jump => {
                let target = self.view.operands(inst).target();
                self.edge(block, target, next)?;
            }
            Op::Br => self.br(inst, block, next)?,
            Op::Switch => self.switch(inst, block, next)?,
            Op::Return => self.ret(inst)?,
            Op::Unreachable => {
                self.put(Pattern { op, width: 0, variant: 0, result: Form::None, operands: [Form::None; 4], inner: None }, &[Arg::None; 7])?;
            }
            Op::Trap => {
                let code = self.view.data[inst];
                if code > 0xffff {
                    return Err(refused(inst, "a trap code past 16 bits"));
                }
                let mut args = [Arg::None; 7];
                args[0] = Arg::Imm(u64::from(code));
                let forms = [Form::Imm(ImmClass::Mov16), Form::None, Form::None, Form::None];
                self.put(Pattern { op, width: 0, variant: 0, result: Form::None, operands: forms, inner: None }, &args)?;
            }
            Op::Fence | Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw | Op::Cmpxchg => self.atomic(inst, op)?,
            Op::TlsAddr => self.tls_addr(inst)?,
            Op::Param
            | Op::Vconst
            | Op::Memcpy
            | Op::Memmove
            | Op::Memset
            | Op::Memcmp
            | Op::MaskedLoad
            | Op::MaskedStore
            | Op::Gather
            | Op::Scatter
            | Op::FrameAddress
            | Op::ReturnAddress
            | Op::StackPointer => return Err(refused(inst, op.name())),
            Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow | Op::UmulOverflow | Op::SmulOverflow => {
                self.overflow(inst, op)?
            }
            Op::Project => self.project(inst)?,
            _ => self.generic(inst, op)?,
        }
        self.after(inst);
        self.scratch_idle()
    }

    /// Between two instructions every scratch register is free: a token not given back is
    /// refused here, never left to overlap the next use (scratch.rs).
    fn scratch_idle(&self) -> Result<(), TierError> {
        match self.scratch.is_idle() {
            true => Ok(()),
            false => Err(refused(self.inst, "a scratch register still held after its instruction (a token not given back)")),
        }
    }

    fn constant(&mut self, inst: usize) -> Result<(), TierError> {
        let (class, _) = self.class_of(Value(inst as u32))?;
        if self.const_bits(inst).is_none() {
            return Err(refused(inst, "a constant wider than 64 bits or of another float than `f32`/`f64`"));
        }
        match self.home(inst as u32) {
            H::Local => self.loc[inst] = Loc::Remat,
            H::Reg(r) => {
                if self.holder[r.0 as usize] != NONE {
                    self.evict(r)?;
                }
                self.materialize(inst, r)?;
                self.loc[inst] = Loc::Reg(r);
            }
            // A constant live across blocks the analysis pass found no register for: it has a
            // cell (`frame`), written once here.
            H::Cell => {
                let bits = self.const_bits(inst).unwrap_or(0);
                match class {
                    RegClass::Gpr => {
                        let s = self.take_scratch()?;
                        self.mov_imm(s.reg(), bits)?;
                        self.store_cell(s.reg(), self.cell[inst])?;
                        self.give_scratch(s)?;
                    }
                    RegClass::Fpr => {
                        let t = self.temp(RegClass::Fpr)?;
                        self.mov_imm(t, bits)?;
                        self.store_cell(t, self.cell[inst])?;
                    }
                }
                self.loc[inst] = Loc::Cell;
            }
        }
        Ok(())
    }

    /// The immediate class and field value of constant `value` as operand `k` of `op` at
    /// `width`, when the operand has one and the value is in it.
    fn imm_class(op: Op, width: u32, k: usize, value: u64) -> Option<(ImmClass, u64)> {
        match (op, k) {
            // T1's table (table_gen/arm64.rs) is the authority on which immediate forms exist:
            // `add`/`sub`/`ptr_add` `U12`; `and`/`or` `Logical64` at every width (operands and
            // the constant are zero-extended); `lshr`/`ashr` `Shift5` at 32 and `Shift6` at 64
            // (no `shl` by an immediate: `lsl` is not linear in its amount). Every other
            // constant operand goes to a register.
            (Op::Add | Op::Sub | Op::PtrAdd, 1) if value < 4096 => Some((ImmClass::U12, value)),
            (Op::And | Op::Or, 1) => crate::enc::logical_imm(value, 64).map(|it| (ImmClass::Logical64, it)),
            (Op::Lshr | Op::Ashr, 1) if value < u64::from(width) => match width {
                32 => Some((ImmClass::Shift5, value)),
                64 => Some((ImmClass::Shift6, value)),
                _ => None,
            },
            _ => None,
        }
    }

    /// An instruction of value operands only: its pattern by op, width and operand forms.
    fn generic(&mut self, inst: usize, op: Op) -> Result<(), TierError> {
        let view = self.view;
        let count = match op.info().layout {
            Layout::None => 0,
            Layout::Data(LirField::Value) => 1,
            Layout::Extra(fields) if fields.len() <= 4 && fields.iter().all(|f| *f == LirField::Value) => fields.len(),
            _ => return Err(refused(inst, op.name())),
        };
        if matches!(op, Op::FptoUi | Op::FptoSi) && view.flags(inst).0 != 0 {
            return Err(refused(inst, "a float-to-integer conversion with flags"));
        }
        let rty = view.ty(Value(inst as u32));
        let result = match rty.is_none() {
            true => None,
            false => Some(self.class_of(Value(inst as u32))?),
        };
        let mut o = view.operands(inst);
        let mut values = [Value(0); 4];
        for value in values.iter_mut().take(count) {
            *value = o.value();
        }
        let width = match result {
            Some((_, w)) => u32::from(w),
            None if count > 0 => u32::from(self.class_of(values[0])?.1),
            None => 0,
        };
        let variant = match op {
            Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast
            | Op::PtrAddr
            | Op::Expose
            | Op::FromExposed => self.class_of(values[0])?.1,
            _ => 0,
        };
        let mut forms = [Form::None; 4];
        let mut args = [Arg::None; 7];
        for k in 0..count {
            let v = values[k];
            let (class, _) = self.class_of(v)?;
            let imm = self.const_bits(v.index()).and_then(|bits| Self::imm_class(op, width, k, bits));
            match imm {
                Some((cls, field)) => {
                    forms[k] = Form::Imm(cls);
                    args[k] = Arg::Imm(field);
                }
                None => {
                    forms[k] = Form::Reg(class);
                    args[k] = Arg::Reg(self.operand(v)?);
                }
            }
        }
        let dest = match result {
            Some((class, _)) => Some(self.dest(class)?),
            None => None,
        };
        if let Some(d) = dest {
            args[RESULT as usize] = Arg::Reg(d.reg());
        }
        let pattern = Pattern {
            op,
            width: width as u8,
            variant,
            result: result.map_or(Form::None, |(class, _)| Form::Reg(class)),
            operands: forms,
            inner: None,
        };
        self.emit_op(pattern, &args, false)?;
        if let Some(d) = dest {
            self.finish_dest(d)?;
        }
        Ok(())
    }

    /// An overflow operation's pair, in its cell as the Reference tier lays it out (pass/int.rs
    /// `overflow`): the wrapped result at +0 (the plain `add`/`sub`/`mul` template), whether it
    /// overflowed at +8 (the flag template, table_gen arm64.rs `overflows`). A pair has no
    /// register class, so it lives in its cell only and each `project` loads its member.
    fn overflow(&mut self, inst: usize, op: Op) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        let (class, width) = self.class_of(a)?;
        if class != RegClass::Gpr {
            return Err(refused(inst, "an overflow operation on a value that is not an integer"));
        }
        let cell = self.cell[inst];
        if cell == NONE {
            return Err(refused(inst, "an overflow operation's pair with no cell"));
        }
        let wrapped = match op {
            Op::UaddOverflow | Op::SaddOverflow => Op::Add,
            Op::UsubOverflow | Op::SsubOverflow => Op::Sub,
            _ => Op::Mul,
        };
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(self.operand(a)?);
        args[1] = Arg::Reg(self.operand(b)?);
        let t = self.temp(RegClass::Gpr)?;
        args[RESULT as usize] = Arg::Reg(t);
        let gpr = Form::Reg(RegClass::Gpr);
        let of = |op| Pattern { op, width, variant: 0, result: gpr, operands: [gpr, gpr, Form::None, Form::None], inner: None };
        self.emit_op(of(wrapped), &args, false)?;
        self.store_cell(t, cell)?;
        self.emit_op(of(op), &args, false)?;
        self.store_cell(t, cell + 8)?;
        self.in_cell[inst] = true;
        self.loc[inst] = Loc::Cell;
        Ok(())
    }

    /// `fence`, `atomic_load`, `atomic_store`, `atomic_rmw` and `cmpxchg` of 8 to 64 bits, by
    /// the Reference tier's Armv8.0 mapping (pass/atomic.rs; the templates are table_gen/arm64.rs
    /// `atomics`, their variants `glue::atomic`). An exclusive loop's status register is a
    /// temporary handed as an operand. `cmpxchg`'s pair lives in its cell like an overflow pair:
    /// the old value at +0, and at +8 whether it was `expected` (an `icmp eq` of the two, both
    /// zero-extended). A single-thread fence emits nothing: this tier keeps every access in
    /// program order. An `i1`, float or wider atomic is the Reference tier's.
    fn atomic(&mut self, inst: usize, op: Op) -> Result<(), TierError> {
        let view = self.view;
        let flags = view.flags(inst);
        let ordering = flags.ordering().ok_or(refused(inst, "an atomic ordering that is none of the five"))?;
        let (acq, rel) = crate::pass::atomic::sides(ordering);
        let gpr = Form::Reg(RegClass::Gpr);
        let n = Form::None;
        let mut o = view.operands(inst);
        let mut args = [Arg::None; 7];
        let pattern = |width, variant, result, operands| Pattern { op, width, variant, result, operands, inner: None };
        match op {
            Op::Fence => {
                if flags.contains(Flags::SINGLE_THREAD) {
                    return Ok(());
                }
                let variant = match ordering {
                    Ordering::Acquire => glue::FENCE_LOADS,
                    _ => glue::FENCE_ALL,
                };
                self.emit_op(pattern(0, variant, n, [n; 4]), &args, false).map(drop)
            }
            Op::AtomicLoad => {
                let ptr = o.value();
                let width = self.atomic_width(inst, Value(inst as u32))?;
                args[0] = Arg::Reg(self.operand(ptr)?);
                let dest = self.dest(RegClass::Gpr)?;
                args[RESULT as usize] = Arg::Reg(dest.reg());
                self.emit_op(pattern(width, glue::atomic(0, acq, false), gpr, [gpr, n, n, n]), &args, false)?;
                self.finish_dest(dest)
            }
            Op::AtomicStore => {
                let (value, ptr) = (o.value(), o.value());
                let width = self.atomic_width(inst, value)?;
                args[0] = Arg::Reg(self.operand(value)?);
                args[1] = Arg::Reg(self.operand(ptr)?);
                self.emit_op(pattern(width, glue::atomic(0, false, rel), n, [gpr, gpr, n, n]), &args, false).map(drop)
            }
            Op::AtomicRmw => {
                let (rmw, ptr, value) = (o.rmw(), o.value(), o.value());
                let width = self.atomic_width(inst, value)?;
                args[0] = Arg::Reg(self.operand(ptr)?);
                args[1] = Arg::Reg(self.operand(value)?);
                args[2] = Arg::Reg(self.temp(RegClass::Gpr)?);
                let dest = self.dest(RegClass::Gpr)?;
                args[RESULT as usize] = Arg::Reg(dest.reg());
                self.emit_op(pattern(width, glue::atomic(rmw as u8, acq, rel), gpr, [gpr, gpr, gpr, n]), &args, false)?;
                self.finish_dest(dest)
            }
            _ => {
                let (ptr, expected, new) = (o.value(), o.value(), o.value());
                let width = self.atomic_width(inst, expected)?;
                let failure = flags.failure().ok_or(refused(inst, "a `cmpxchg` failure ordering that is none of the five"))?;
                // The acquire part from the stronger of success and failure.
                let acq = acq || crate::pass::atomic::sides(failure).0;
                let cell = self.cell[inst];
                if cell == NONE {
                    return Err(refused(inst, "a `cmpxchg` pair with no cell"));
                }
                let e = self.operand(expected)?;
                args[0] = Arg::Reg(self.operand(ptr)?);
                args[1] = Arg::Reg(e);
                args[2] = Arg::Reg(self.operand(new)?);
                args[3] = Arg::Reg(self.temp(RegClass::Gpr)?);
                let t = self.temp(RegClass::Gpr)?;
                args[RESULT as usize] = Arg::Reg(t);
                self.emit_op(pattern(width, glue::atomic(0, acq, rel), gpr, [gpr; 4]), &args, false)?;
                self.store_cell(t, cell)?;
                let mut same = [Arg::None; 7];
                same[0] = Arg::Reg(t);
                same[1] = Arg::Reg(e);
                same[RESULT as usize] = Arg::Reg(t);
                let eq = Pattern { op: Op::Icmp, width, variant: IntCond::Eq as u8, result: gpr, operands: [gpr, gpr, n, n], inner: None };
                self.emit_op(eq, &same, false)?;
                self.store_cell(t, cell + 8)?;
                self.in_cell[inst] = true;
                self.loc[inst] = Loc::Cell;
                Ok(())
            }
        }
    }

    /// `tls_addr` of a thread-local: the Reference tier's indexed access (pass/tls_index.rs) in
    /// the two templates of table_gen/arm64.rs `thread_locals`. The descriptor's, the index's and
    /// the getter's import words are loaded first ([`Composer::import_word`]), then the root's
    /// address on the table's OS, then the lookup, emitted as a call (the getter's) so what lives
    /// in a caller-saved register is kept across it; the thread's copy is moved from x0 last.
    fn tls_addr(&mut self, inst: usize) -> Result<(), TierError> {
        let rec = self.view.reference(self.view.operands(inst).reference());
        if RefKind::from_word(rec.kind) != Some(RefKind::Tls) {
            return Err(refused(inst, "`tls_addr` of other than a thread-local"));
        }
        let desc = self.import_word(Outside::TlsDesc, Space::Static)?;
        let index = self.import_word(Outside::Tls(rec.place), Space::Tls)?;
        let getter = self.import_word(Outside::TlsGetter, Space::Func)?;
        let os = self.table.head().machine.os;
        let gpr = Form::Reg(RegClass::Gpr);
        let n = Form::None;
        let at = self.temp(RegClass::Gpr)?;
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(desc);
        args[RESULT as usize] = Arg::Reg(at);
        let root = Pattern { op: Op::TlsAddr, width: 64, variant: glue::tls_root(os), result: gpr, operands: [gpr, n, n, n], inner: None };
        self.emit_op(root, &args, false)?;
        let mut args = [Arg::None; 7];
        for (k, r) in [at, index, getter, desc].into_iter().enumerate() {
            args[k] = Arg::Reg(r);
        }
        let lookup = Pattern { op: Op::TlsAddr, width: 64, variant: glue::TLS_LOOKUP, result: n, operands: [gpr; 4], inner: None };
        self.emit_op(lookup, &args, true)?;
        // The template leaves the copy in x0, the getter's return register.
        let x0 = self.regs.rets_gpr.first().copied().ok_or(refused(inst, "a description with no return register"))?;
        let dest = self.dest(RegClass::Gpr)?;
        self.mov(dest.reg(), x0)?;
        self.finish_dest(dest)
    }

    /// The value of `of`'s import word (`adrp; ldr`: [`glue::address`]) in a temporary.
    fn import_word(&mut self, of: Outside, space: Space) -> Result<Reg, TierError> {
        let target = self.target_of(of, space)?;
        let r = self.temp(RegClass::Gpr)?;
        let mut args = [Arg::None; 7];
        args[0] = Arg::Site(target);
        args[RESULT as usize] = Arg::Reg(r);
        self.emit_op(glue::address(Op::DataAddr), &args, false)?;
        Ok(r)
    }

    /// The width of an atomic's value: 8, 16, 32 or 64 bits of an integer or pointer.
    fn atomic_width(&self, inst: usize, v: Value) -> Result<u8, TierError> {
        match self.class_of(v)? {
            (RegClass::Gpr, w @ (8 | 16 | 32 | 64)) => Ok(w),
            _ => Err(refused(inst, "an atomic of an `i1` or a float (the Reference tier's)")),
        }
    }

    /// `project(pair, k)`: member `k` of an overflow operation's or a `cmpxchg`'s pair, loaded
    /// from the pair's cell ([`Composer::overflow`], [`Composer::atomic`]). Any other compound
    /// is the Reference tier's.
    fn project(&mut self, inst: usize) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (pair, k) = (o.value(), o.imm());
        let from = pair.index();
        let pairs =
            [Op::UaddOverflow, Op::SaddOverflow, Op::UsubOverflow, Op::SsubOverflow, Op::UmulOverflow, Op::SmulOverflow, Op::Cmpxchg];
        if from >= self.view.len() || !pairs.contains(&self.view.op(from)) || k > 1 {
            return Err(refused(inst, "a member of a compound that is not an overflow operation's or a `cmpxchg`'s pair (the Reference tier's)"));
        }
        let cell = self.cell[from];
        if cell == NONE {
            return Err(refused(inst, "a member of a pair with no cell"));
        }
        let (class, _) = self.class_of(Value(inst as u32))?;
        let dest = self.dest(class)?;
        self.load_cell(dest.reg(), cell + 8 * k)?;
        self.finish_dest(dest)
    }

    /// The right side of an integer comparison of `width` bits: an `U12` immediate when the
    /// constant, extended as the condition reads it, is in 0..4096.
    fn cmp_rhs(&mut self, cond: IntCond, width: u32, b: Value) -> Result<(Form, Arg), TierError> {
        if let Some(bits) = self.const_bits(b.index()) {
            let value = match signed(cond) && width < 64 && bits >> (width - 1) & 1 == 1 {
                true => None,
                false => Some(bits),
            };
            if let Some(value) = value.filter(|&it| it < 4096) {
                return Ok((Form::Imm(ImmClass::U12), Arg::Imm(value)));
            }
        }
        Ok((Form::Reg(RegClass::Gpr), Arg::Reg(self.operand(b)?)))
    }

    fn icmp(&mut self, inst: usize) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (cond, a, b) = (o.int_cond(), o.value(), o.value());
        let (class, width) = self.class_of(a)?;
        if class != RegClass::Gpr {
            return Err(refused(inst, "an `icmp` of floats"));
        }
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(self.operand(a)?);
        let (rhs, arg) = self.cmp_rhs(cond, u32::from(width), b)?;
        args[1] = arg;
        let dest = self.dest(RegClass::Gpr)?;
        args[RESULT as usize] = Arg::Reg(dest.reg());
        let forms = [Form::Reg(RegClass::Gpr), rhs, Form::None, Form::None];
        let pattern = Pattern { op: Op::Icmp, width, variant: cond as u8, result: Form::Reg(RegClass::Gpr), operands: forms, inner: None };
        self.emit_op(pattern, &args, false)?;
        self.finish_dest(dest)
    }

    fn fcmp(&mut self, inst: usize) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (cond, a, b) = (o.float_cond(), o.value(), o.value());
        let (class, width) = self.class_of(a)?;
        if class != RegClass::Fpr {
            return Err(refused(inst, "an `fcmp` of integers"));
        }
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(self.operand(a)?);
        args[1] = Arg::Reg(self.operand(b)?);
        let dest = self.dest(RegClass::Gpr)?;
        args[RESULT as usize] = Arg::Reg(dest.reg());
        let forms = [Form::Reg(RegClass::Fpr), Form::Reg(RegClass::Fpr), Form::None, Form::None];
        let pattern = Pattern { op: Op::Fcmp, width, variant: cond as u8, result: Form::Reg(RegClass::Gpr), operands: forms, inner: None };
        self.emit_op(pattern, &args, false)?;
        self.finish_dest(dest)
    }

    /// A load or store offset: `U12Scaled{k}` when a multiple of the access below 4096 units,
    /// else `S9` when in -256..256, else in a register.
    fn offset(&mut self, off: i32, width: u8) -> Result<(Form, Arg), TierError> {
        let bytes = u32::from(width).div_ceil(8).max(1);
        let k = bytes.trailing_zeros();
        const SCALED: [ImmClass; 5] =
            [ImmClass::U12Scaled0, ImmClass::U12Scaled1, ImmClass::U12Scaled2, ImmClass::U12Scaled3, ImmClass::U12Scaled4];
        if off >= 0 && (off as u32) % bytes == 0 && (off as u32 / bytes) < 4096 && (k as usize) < SCALED.len() {
            return Ok((Form::Imm(SCALED[k as usize]), Arg::Imm(u64::from(off as u32 / bytes))));
        }
        if (-256..256).contains(&off) {
            return Ok((Form::Imm(ImmClass::S9), Arg::Imm((off as i64 as u64) & 0x1ff)));
        }
        let r = self.temp(RegClass::Gpr)?;
        self.mov_imm(r, off as i64 as u64)?;
        Ok((Form::Reg(RegClass::Gpr), Arg::Reg(r)))
    }

    /// The base and index of the fused `ptr_add` `ptr`, in registers.
    fn fused_address(&mut self, ptr: Value) -> Result<(Reg, Reg), TierError> {
        let mut i = self.view.operands(ptr.index());
        let (base, index) = (i.value(), i.value());
        Ok((self.operand(base)?, self.operand(index)?))
    }

    fn load(&mut self, inst: usize) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (ptr, off) = (o.value(), o.offset());
        let (class, width) = self.access_width(Value(inst as u32))?;
        let mut args = [Arg::None; 7];
        let (forms, inner) = match self.fused[ptr.index()] {
            true => {
                let (base, index) = self.fused_address(ptr)?;
                args[1] = Arg::Reg(base);
                args[2] = Arg::Reg(index);
                let forms = [Form::None, Form::Reg(RegClass::Gpr), Form::Reg(RegClass::Gpr), Form::None];
                (forms, Some(Inner { op: Op::PtrAdd, variant: 0, operand: 0 }))
            }
            false => {
                args[0] = Arg::Reg(self.operand(ptr)?);
                let (form, arg) = self.offset(off, width)?;
                args[1] = arg;
                ([Form::Reg(RegClass::Gpr), form, Form::None, Form::None], None)
            }
        };
        let dest = self.dest(class)?;
        args[RESULT as usize] = Arg::Reg(dest.reg());
        let pattern = Pattern { op: Op::Load, width, variant: 0, result: Form::Reg(class), operands: forms, inner };
        self.emit_op(pattern, &args, false)?;
        self.finish_dest(dest)
    }

    fn store(&mut self, inst: usize) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (value, ptr, off) = (o.value(), o.value(), o.offset());
        let (class, width) = self.access_width(value)?;
        let mut args = [Arg::None; 7];
        args[0] = Arg::Reg(self.operand(value)?);
        let (forms, inner) = match self.fused[ptr.index()] {
            true => {
                let (base, index) = self.fused_address(ptr)?;
                args[2] = Arg::Reg(base);
                args[3] = Arg::Reg(index);
                let forms = [Form::Reg(class), Form::None, Form::Reg(RegClass::Gpr), Form::Reg(RegClass::Gpr)];
                (forms, Some(Inner { op: Op::PtrAdd, variant: 0, operand: 1 }))
            }
            false => {
                args[1] = Arg::Reg(self.operand(ptr)?);
                let (form, arg) = self.offset(off, width)?;
                args[2] = arg;
                ([Form::Reg(class), Form::Reg(RegClass::Gpr), form, Form::None], None)
            }
        };
        let pattern = Pattern { op: Op::Store, width, variant: 0, result: Form::None, operands: forms, inner };
        self.emit_op(pattern, &args, false).map(drop)
    }

    fn slot_addr(&mut self, inst: usize) -> Result<(), TierError> {
        let slot = self.view.operands(inst).slot();
        let off = self.slot_at.get(slot.index()).copied().ok_or(refused(inst, "a slot the function lacks"))?;
        let dest = self.dest(RegClass::Gpr)?;
        let mut args = [Arg::None; 7];
        args[0] = Arg::Frame(off);
        args[RESULT as usize] = Arg::Reg(dest.reg());
        let pattern = match off < SLOT_REACH {
            true => glue::slot_address(),
            false => glue::slot_address_far(),
        };
        self.emit_op(pattern, &args, false)?;
        self.finish_dest(dest)
    }

    /// The record's index of `of`'s target, added once.
    fn target_of(&mut self, of: Outside, space: Space) -> Result<u32, TierError> {
        let named = self.names.name(of).ok_or(TierError::NoName { tier: TIER, of })?;
        let (target, name) = match named {
            Named::Symbol(symbol) => (Target { space, symbol }, None),
            Named::C(name) => match space {
                Space::Func | Space::Static => (Target { space: Space::Outside, symbol: Symbol::of_name(name) }, Some(name)),
                _ => return Err(refused(self.inst, "a C name for a vtable or a thread-local")),
            },
        };
        Ok(self.intern(target, name))
    }

    fn intern(&mut self, target: Target, name: Option<&str>) -> u32 {
        if let Some(at) = self.out.targets.iter().position(|t| *t == target) {
            return at as u32;
        }
        let at = self.out.targets.len() as u32;
        self.out.targets.push(target);
        if let Some(name) = name {
            // A name row is its text's start and LENGTH (record/form.rs `seal`, record/v1.rs).
            let from = self.out.text.len() as u32;
            self.out.text.extend_from_slice(name.as_bytes());
            self.out.names.push((target.symbol, from, name.len() as u32));
        }
        at
    }

    fn address(&mut self, inst: usize, op: Op) -> Result<(), TierError> {
        let r = self.view.operands(inst).reference();
        let rec = self.view.reference(r);
        let (of, space) = match RefKind::from_word(rec.kind) {
            Some(RefKind::Func) => (Outside::Func(rec.place), Space::Func),
            Some(RefKind::Static) => (Outside::Static(rec.place), Space::Static),
            Some(RefKind::Vtable) => (Outside::Vtable(rec.place), Space::Vtable),
            Some(RefKind::Data) => return self.own_address(rec.place),
            Some(RefKind::Tls) | None => return Err(refused(inst, "the address of a thread-local (the Reference tier's)")),
        };
        let target = self.target_of(of, space)?;
        let dest = self.dest(RegClass::Gpr)?;
        let mut args = [Arg::None; 7];
        args[0] = Arg::Site(target);
        args[RESULT as usize] = Arg::Reg(dest.reg());
        self.emit_op(glue::address(op), &args, false)?;
        self.finish_dest(dest)
    }

    /// `data_addr` of the function's own data row `row`: an own-data site ([`glue::own_address`])
    /// reaching the row, which is placed after the code ([`Composer::place_rows`]).
    fn own_address(&mut self, row: u32) -> Result<(), TierError> {
        let index = self.place_row(row)?;
        let dest = self.dest(RegClass::Gpr)?;
        let mut args = [Arg::None; 7];
        // For a `Site(Relro)` field, the placed row's index (`place`), not a target's.
        args[0] = Arg::Site(index);
        args[RESULT as usize] = Arg::Reg(dest.reg());
        self.emit_op(glue::own_address(), &args, false)?;
        self.finish_dest(dest)
    }

    /// Row `row`'s index among the rows placed after the code, added once.
    fn place_row(&mut self, row: u32) -> Result<u32, TierError> {
        let Some(&index) = self.row_index.get(row as usize) else {
            return Err(refused(self.inst, "a reference to own data naming no row of the function's"));
        };
        if index != NONE {
            return Ok(index);
        }
        let index = self.rows.len() as u32;
        self.rows.push(row);
        self.row_index[row as usize] = index;
        Ok(index)
    }

    /// The rows of the function's own data, after the code: a row holding no address and
    /// aligned to at most 4 bytes follows the instructions in the code part; any other row goes
    /// to the relro part, at least 8-aligned, its addresses relro words, a row aligned past 8
    /// raising the part's alignment (the record's `relro_align`, which the placer honours: up
    /// to a page, as the Reference tier's). Each source position in a row is a slot of the
    /// record (`position_slots`) the placer fills. Then each own-data site and each word naming
    /// an own row gets the row's part and offset.
    fn place_rows(&mut self) -> Result<(), TierError> {
        const CODE_PART_ALIGN: u32 = 2;
        const RELRO_PART_ALIGN: u32 = 3;
        let view = self.view;
        let mut k = 0;
        // The list grows as it is walked: a row reached only from another row's address.
        while k < self.rows.len() {
            let row = self.rows[k];
            let id = DataId(row);
            let rec = view.datas[row as usize];
            let relocs = view.data_relocs_of(id);
            let bytes = view.data_bytes_of(id);
            let in_relro = !relocs.is_empty() || rec.align > CODE_PART_ALIGN;
            if rec.align > crate::pass::MOST_DATA_ALIGN {
                return Err(refused_fn(crate::pass::DATA_ALIGN_PAST));
            }
            // A relro row past 8 bytes' alignment raises the part's (the record's `relro_align`,
            // which the placer honours); a code row is at most 4-aligned, under the code's 16.
            if in_relro {
                self.out.relro_align = self.out.relro_align.max(rec.align as u8);
            }
            let part = match in_relro {
                true => &mut self.out.relro,
                false => &mut self.out.code,
            };
            let align = match in_relro {
                true => 1usize << rec.align.max(RELRO_PART_ALIGN),
                false => 1usize << rec.align,
            };
            while part.len() % align != 0 {
                part.push(0);
            }
            let start = part.len();
            if start + bytes.len() >= (u32::MAX / 2) as usize {
                return Err(too_large("the function's own data (2 GiB)"));
            }
            part.extend_from_slice(bytes);
            let start = start as u32;
            // Each source position of the row: its four bytes zero, a slot of the record the
            // placer fills with the run's line or column (as the Reference tier's, pass/sites.rs).
            for p in view.data_positions_of(id) {
                let field = match blaze_lir::PositionField::from_word(p.field) {
                    Some(blaze_lir::PositionField::Line) => PositionField::Line,
                    Some(blaze_lir::PositionField::Col) => PositionField::Col,
                    None => return Err(refused_fn("a source position of no known field")),
                };
                if u64::from(p.offset) + 4 > u64::from(rec.bytes_len) {
                    return Err(refused_fn("a source position past its row's end"));
                }
                let at = start + p.offset;
                let (part, slot_part) = match in_relro {
                    true => (&mut self.out.relro, PositionPart::Relro),
                    false => (&mut self.out.code, PositionPart::Code),
                };
                part[at as usize..at as usize + 4].fill(0);
                self.out.position_slots.push(PositionRow { part: slot_part, at, item: p.item_key(), site: p.site, field });
            }
            self.row_at.push((in_relro, start));
            for r in relocs {
                if u64::from(r.offset) + 8 > u64::from(rec.bytes_len) {
                    return Err(refused_fn("an address past its data row's end"));
                }
                if r.offset % 8 != 0 {
                    return Err(refused_fn(crate::pass::sites::UNALIGNED_ADDRESS));
                }
                let word = start + r.offset;
                // The placer writes the whole word: the record's bytes there are zero.
                self.out.relro[word as usize..word as usize + 8].fill(0);
                let target = view.reference(RefId(r.target));
                let (of, space) = match RefKind::from_word(target.kind) {
                    Some(RefKind::Static) => (Outside::Static(target.place), Space::Static),
                    Some(RefKind::Vtable) => (Outside::Vtable(target.place), Space::Vtable),
                    Some(RefKind::Func) => (Outside::Func(target.place), Space::Func),
                    Some(RefKind::Data) => {
                        let index = self.place_row(target.place)?;
                        self.inner.push((word, index, r.addend));
                        continue;
                    }
                    Some(RefKind::Tls) | None => return Err(refused_fn("the address of a thread-local in data (it differs per thread)")),
                };
                let t = self.target_of(of, space)?;
                if self.out.targets[t as usize].space == Space::Outside && r.addend != 0 {
                    return Err(refused_fn("data holding a C name's address plus an addend"));
                }
                self.out.words.push(RelroWord { at: word, target: t, addend: r.addend });
            }
            k += 1;
        }
        for i in 0..self.inner.len() {
            let (word, index, addend) = self.inner[i];
            let (in_relro, start) = self.row_at[index as usize];
            let addend = i64::from(start).checked_add(addend).ok_or(too_large("an own-data word's addend"))?;
            let target = if in_relro { OWN_RELRO } else { OWN_CODE };
            self.out.words.push(RelroWord { at: word, target, addend });
        }
        // Offsets are distinct (each address its own word), so the unstable sort is exact.
        self.out.words.sort_unstable_by_key(|w| w.at);
        for i in 0..self.own_sites.len() {
            let (reloc, index) = self.own_sites[i];
            let (in_relro, start) = self.row_at[index as usize];
            let target = if in_relro { OWN_RELRO } else { OWN_CODE };
            for r in [reloc as usize, reloc as usize + 1] {
                self.out.relocs[r].target = target;
                self.out.relocs[r].addend = i64::from(start);
            }
        }
        Ok(())
    }

    /// A call in the C convention (AAPCS64; Apple's `signext` rule): scalar arguments in their
    /// registers, at most one scalar result; live caller-saved values kept around it; a
    /// `Continue` unwind row at its `blr`.
    fn call(&mut self, inst: usize, op: Op) -> Result<(), TierError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let (callee, sig) = match op {
            Op::Call => (None, (o.reference(), o.sig()).1),
            _ => {
                let f = o.value();
                (Some(f), o.sig())
            }
        };
        let reference = match op {
            Op::Call => Some(view.operands(inst).reference()),
            _ => None,
        };
        let args = o.values();
        if view.sigs[sig.index()].bits & blaze_lir::func::SigRec::VARIADIC != 0 && args.len() > view.sig(sig).params.len() {
            return Err(refused(
                inst,
                "a call passing variable arguments to a C-variadic callee (Apple stacks them in an outgoing area opened before the call site; the Reference tier's pass/variadic.rs builds it)",
            ));
        }
        let sig = view.sig(sig);
        if sig.params.len() != args.len() {
            return Err(refused(inst, "a call whose arguments are not its signature's parameters"));
        }
        if sig.rets.len() > 1 {
            return Err(refused(inst, "a call returning more than one value (the Reference tier's)"));
        }
        let (mut g, mut f) = (0usize, 0usize);
        for (v, param) in args.iter().zip(sig.params.iter()) {
            if param.kind != ParamKind::Value as u8 {
                return Err(refused(inst, "an `sret` or `byval` argument (the Reference tier's)"));
            }
            let (class, width) = self.class_of(v)?;
            let dst = match class {
                RegClass::Gpr => self.regs.args_gpr.get(g).copied().inspect(|_| g += 1),
                RegClass::Fpr => self.regs.args_fpr.get(f).copied().inspect(|_| f += 1),
            };
            let dst = dst.ok_or(refused(inst, "a call with arguments past the registers"))?;
            self.value_into(v, dst)?;
            if class == RegClass::Gpr && width < 32 && param.ext == Ext::Sext as u8 {
                let mut a = [Arg::None; 7];
                a[0] = Arg::Reg(dst);
                a[RESULT as usize] = Arg::Reg(dst);
                self.put(glue::sext32(width), &a)?;
            }
        }
        let mut a = [Arg::None; 7];
        // An indirect callee not in a register is loaded into a scratch register held over the
        // call's own spills: they and the call template see it held (a far spill's address is
        // lent the other one), and the call, which clobbers it after its `blr` reads it, spends
        // it (scratch.rs).
        let mut held: Option<Scratch> = None;
        let pattern = match (reference, callee) {
            (Some(r), _) => {
                let rec = view.reference(r);
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    return Err(refused(inst, "a call of a reference that is not a function"));
                }
                a[0] = Arg::Site(self.target_of(Outside::Func(rec.place), Space::Func)?);
                glue::call()
            }
            (None, Some(fv)) => {
                let reg = match self.src_of(fv)? {
                    Src::Reg(r) => r,
                    _ => {
                        let s = self.take_scratch()?;
                        let reg = s.reg();
                        held = Some(s);
                        self.value_into(fv, reg)?;
                        reg
                    }
                };
                a[0] = Arg::Reg(reg);
                glue::call_indirect()
            }
            (None, None) => return Err(refused(inst, "a call with no callee")),
        };
        let (start, len) = self.emit_op(pattern, &a, true)?;
        if let Some(s) = held {
            self.give_scratch(s)?;
        }
        let row = Row { start: start + len - INST, len: INST, landing: 0, action: Action::Continue };
        self.out.unwind.push(row);
        self.out.pads.push(row.pad());
        if let Some(ret) = sig.rets.first() {
            let _ = ret;
            let (class, width) = self.class_of(Value(inst as u32))?;
            let src = match class {
                RegClass::Gpr => self.regs.rets_gpr.first().copied(),
                RegClass::Fpr => self.regs.rets_fpr.first().copied(),
            };
            let src = src.ok_or(refused(inst, "a description with no return register"))?;
            let dest = self.dest(class)?;
            match class == RegClass::Gpr && width < 64 {
                true => self.narrow(dest.reg(), src, width)?,
                false => self.mov(dest.reg(), src)?,
            }
            self.finish_dest(dest)?;
        }
        Ok(())
    }

    fn ret(&mut self, inst: usize) -> Result<(), TierError> {
        let view = self.view;
        let values = view.operands(inst).values();
        let sig = view.sig(SigId(view.head.sig));
        if values.len() != sig.rets.len() {
            return Err(refused(inst, "a return whose values are not the signature's"));
        }
        let (mut g, mut f) = (0usize, 0usize);
        for (v, ret) in values.iter().zip(sig.rets.iter()) {
            if ret.kind != ParamKind::Value as u8 {
                return Err(refused(inst, "an indirect return (the Reference tier's)"));
            }
            let (class, width) = self.class_of(v)?;
            let dst = match class {
                RegClass::Gpr => self.regs.rets_gpr.get(g).copied().inspect(|_| g += 1),
                RegClass::Fpr => self.regs.rets_fpr.get(f).copied().inspect(|_| f += 1),
            };
            let dst = dst.ok_or(refused(inst, "a return past the registers"))?;
            self.value_into(v, dst)?;
            if class == RegClass::Gpr && width < 32 && ret.ext == Ext::Sext as u8 {
                let mut a = [Arg::None; 7];
                a[0] = Arg::Reg(dst);
                a[RESULT as usize] = Arg::Reg(dst);
                self.put(glue::sext32(width), &a)?;
            }
        }
        let mut a = [Arg::None; 7];
        a[0] = Arg::Imm(16 * self.pairs.len() as u64);
        self.put(glue::frame(glue::SP_FROM_FP, [Form::Imm(ImmClass::U12), Form::None, Form::None, Form::None]), &a)?;
        let mut k = self.pairs.len();
        while k > 0 {
            k -= 1;
            let (r1, r2) = self.pairs[k];
            let form = Form::Reg(r1.class());
            let mut a = [Arg::None; 7];
            a[0] = Arg::Reg(r1);
            a[1] = Arg::Reg(r2);
            self.put(glue::frame(glue::RESTORE_PAIR, [form, form, Form::None, Form::None]), &a)?;
        }
        self.put(glue::frame(glue::LEAVE, [Form::None; 4]), &[Arg::None; 7])?;
        self.put(glue::ret(), &[Arg::None; 7]).map(drop)
    }

    // ---- edges -----------------------------------------------------------------------------

    fn back_edge(&self, from: Block, to: Block) -> Option<u32> {
        match self.an.back_edges.iter().any(|&(f, h)| f == from && h == to) {
            true => Some(self.header_loop[to.index()]),
            false => None,
        }
    }

    fn needs_stub(&self, from: Block, to: &Edge<'_>) -> bool {
        !to.args.is_empty() || self.back_edge(from, to.block).is_some()
    }

    fn label(&mut self) -> u32 {
        self.labels.push(NONE);
        LABEL | (self.labels.len() as u32 - 1)
    }

    fn bind(&mut self, label: u32) {
        self.labels[(label & !LABEL) as usize] = self.out.code.len() as u32;
    }

    fn jump(&mut self, to: u32) -> Result<(), TierError> {
        let mut a = [Arg::None; 7];
        a[0] = Arg::To(to);
        self.put(glue::jump(), &a).map(drop)
    }

    /// One edge: the parallel copy, the poll on a back edge, the jump unless `to` is next.
    fn edge(&mut self, from: Block, to: Edge<'_>, next: Option<Block>) -> Result<(), TierError> {
        self.parallel_copy(&to)?;
        if let Some(row) = self.back_edge(from, to.block) {
            self.poll(row)?;
        }
        if next != Some(to.block) {
            self.jump(to.block.0)?;
        }
        Ok(())
    }

    fn poll(&mut self, row: u32) -> Result<(), TierError> {
        if self.poll_target == NONE {
            let target = Target { space: Space::Outside, symbol: Symbol::of_name(POLL_PAGE) };
            self.poll_target = self.intern(target, Some(POLL_PAGE));
        }
        let at = self.put(glue::poll(), &[Arg::None; 7])?;
        let target = self.poll_target;
        self.out.relocs.push(Reloc { at, in_relro: false, kind: RelocKind::Page21, target, addend: 0 });
        self.out.relocs.push(Reloc { at: at + INST, in_relro: false, kind: RelocKind::PageOff12 { scale: 3 }, target, addend: 0 });
        self.out.polls.push(Poll { at, header: row });
        Ok(())
    }

    /// The target's parameters take the edge's arguments at once: moves whose destination no
    /// other move still reads go first; a cycle is broken by copying one source to the spare
    /// cell (one at a time: a parameter has one writer, so the moves are trees hanging off
    /// disjoint cycles, and a broken cycle's moves all become ready before another is broken;
    /// a second break while the spare cell is still read is refused by name); constants last.
    /// No scratch register is held from one move to the next.
    fn parallel_copy(&mut self, to: &Edge<'_>) -> Result<(), TierError> {
        let params = self.view.params(to.block);
        if params.len() != to.args.len() {
            return Err(refused(self.inst, "an edge whose arguments are not its target's parameters"));
        }
        self.moves.clear();
        for (param, v) in params.zip(to.args.iter()) {
            let (class, _) = self.class_of(Value(param as u32))?;
            let dst = match self.home(param as u32) {
                H::Reg(r) => Dst::Reg(r),
                H::Cell | H::Local => Dst::Cell(self.cell[param]),
            };
            let src = self.src_of(v)?;
            let same = match (src, dst) {
                (Src::Reg(a), Dst::Reg(b)) => a == b,
                (Src::Cell(a), Dst::Cell(b)) => a == b,
                _ => false,
            };
            if !same {
                self.moves.push(Move { src, dst, class });
            }
        }
        loop {
            let pending = self.moves.iter().filter(|m| !matches!(m.src, Src::Const(_))).count();
            if pending == 0 {
                break;
            }
            let ready = (0..self.moves.len()).find(|&i| {
                let m = self.moves[i];
                !matches!(m.src, Src::Const(_))
                    && !self.moves.iter().enumerate().any(|(j, o)| {
                        j != i
                            && match (o.src, m.dst) {
                                (Src::Reg(a), Dst::Reg(b)) => a == b,
                                (Src::Cell(a), Dst::Cell(b)) => a == b,
                                _ => false,
                            }
                    })
            });
            match ready {
                Some(i) => {
                    let m = self.moves.remove(i);
                    self.one_move(m.src, m.dst, m.class)?;
                }
                None => {
                    let Some(i) = self.moves.iter().position(|m| !matches!(m.src, Src::Const(_))) else { break };
                    let spare = self.spare;
                    if spare == NONE {
                        return Err(refused(self.inst, "a cycle of moves in a function with no spare cell (no block has two parameters)"));
                    }
                    if self.moves.iter().any(|m| m.src == Src::Cell(spare)) {
                        return Err(refused(self.inst, "a second cycle of moves broken while the spare cell is still read"));
                    }
                    let m = self.moves[i];
                    self.one_move(m.src, Dst::Cell(spare), m.class)?;
                    self.moves[i].src = Src::Cell(spare);
                }
            }
        }
        let mut k = 0;
        while k < self.moves.len() {
            let m = self.moves[k];
            if let Src::Const(_) = m.src {
                self.one_move(m.src, m.dst, m.class)?;
            }
            k += 1;
        }
        self.moves.clear();
        Ok(())
    }

    /// One move of the parallel copy. A cell-to-cell move takes a scratch register for its load
    /// and its store and gives it back; a far cell's address is the templates' own temporary.
    /// A cell holds 64 bits whatever the class, so the copy goes through a general register.
    fn one_move(&mut self, src: Src, dst: Dst, class: RegClass) -> Result<(), TierError> {
        let _ = class;
        match (src, dst) {
            (Src::Reg(a), Dst::Reg(b)) => self.mov(b, a),
            (Src::Reg(a), Dst::Cell(off)) => self.store_cell(a, off),
            (Src::Cell(off), Dst::Reg(b)) => self.load_cell(b, off),
            (Src::Cell(from), Dst::Cell(to)) => {
                let s = self.take_scratch()?;
                self.load_cell(s.reg(), from)?;
                self.store_cell(s.reg(), to)?;
                self.give_scratch(s)
            }
            (Src::Const(c), Dst::Reg(r)) => self.materialize(c as usize, r),
            (Src::Const(c), Dst::Cell(off)) => {
                let bits = self.const_bits(c as usize).ok_or(refused(self.inst, "a constant with no bits"))?;
                let s = self.take_scratch()?;
                self.mov_imm(s.reg(), bits)?;
                self.store_cell(s.reg(), off)?;
                self.give_scratch(s)
            }
        }
    }

    /// A branch to `to` when `cond` (or, `invert`ed, when not): a fused compare when `cond` is
    /// a fused `icmp`, else `cbnz`/`cbz`.
    fn cond_branch(&mut self, cond: Value, invert: bool, to: u32) -> Result<(), TierError> {
        let mut a = [Arg::None; 7];
        a[1] = Arg::To(to);
        let pattern = match self.fused[cond.index()] {
            true => {
                let mut i = self.view.operands(cond.index());
                let (c, x, y) = (i.int_cond(), i.value(), i.value());
                let (_, width) = self.class_of(x)?;
                let c = if invert { inverse(c) } else { c };
                a[2] = Arg::Reg(self.operand(x)?);
                let (rhs, arg) = self.cmp_rhs(c, u32::from(width), y)?;
                a[3] = arg;
                glue::compare_branch(width, c, rhs)
            }
            false if invert => return Err(refused(self.inst, "an inverted unfused branch (the table has no `cbz`)")),
            false => {
                a[0] = Arg::Reg(self.operand(cond)?);
                glue::branch()
            }
        };
        self.put(pattern, &a).map(drop)
    }

    fn br(&mut self, inst: usize, block: Block, next: Option<Block>) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let (cond, then, other) = (o.value(), o.target(), o.target());
        let (then_stub, other_stub) = (self.needs_stub(block, &then), self.needs_stub(block, &other));
        // Inverted only when fused (the condition is inverted in the compare); the table serves
        // `cbnz` and no `cbz` (T1), so an unfused branch keeps its sense.
        if !then_stub && !other_stub && next == Some(then.block) && self.fused[cond.index()] {
            return self.cond_branch(cond, true, other.block.0);
        }
        // The then edge has moves and the else edge none: branch on the inverse straight to the
        // else block and run the then edge inline (no stub, no jump over it).
        if then_stub && !other_stub && self.fused[cond.index()] {
            self.cond_branch(cond, true, other.block.0)?;
            return self.edge(block, then, next);
        }
        let to = match then_stub {
            true => self.label(),
            false => then.block.0,
        };
        self.cond_branch(cond, false, to)?;
        self.edge(block, other, if then_stub { None } else { next })?;
        if then_stub {
            self.bind(to);
            self.edge(block, then, next)?;
        }
        Ok(())
    }

    /// `switch`: one fused compare-for-equality and branch per case, then the default edge,
    /// then the stubs of the cases that move arguments or poll.
    fn switch(&mut self, inst: usize, block: Block, next: Option<Block>) -> Result<(), TierError> {
        let mut o = self.view.operands(inst);
        let scrutinee = o.value();
        let default = o.target();
        let cases = o.cases(scrutinee);
        let (class, width) = self.class_of(scrutinee)?;
        if class != RegClass::Gpr {
            return Err(refused(inst, "a `switch` on a float"));
        }
        let s = self.operand(scrutinee)?;
        let mut wide: Option<Reg> = None;
        let mut stubs = false;
        let first_label = self.labels.len() as u32;
        for (words, target) in cases {
            let value = u64::from(words.first().copied().unwrap_or(0)) | (u64::from(words.get(1).copied().unwrap_or(0)) << 32);
            let to = match self.needs_stub(block, &target) {
                true => {
                    stubs = true;
                    self.label()
                }
                false => target.block.0,
            };
            let mut a = [Arg::None; 7];
            a[1] = Arg::To(to);
            a[2] = Arg::Reg(s);
            let rhs = match value < 4096 {
                true => {
                    a[3] = Arg::Imm(value);
                    Form::Imm(ImmClass::U12)
                }
                false => {
                    let r = match wide {
                        Some(r) => r,
                        None => {
                            let r = self.temp(RegClass::Gpr)?;
                            wide = Some(r);
                            r
                        }
                    };
                    self.mov_imm(r, value)?;
                    a[3] = Arg::Reg(r);
                    Form::Reg(RegClass::Gpr)
                }
            };
            self.put(glue::compare_branch(width, IntCond::Eq, rhs), &a)?;
        }
        self.edge(block, default, if stubs { None } else { next })?;
        let mut label = first_label;
        for (_, target) in cases {
            if !self.needs_stub(block, &target) {
                continue;
            }
            self.bind(LABEL | label);
            label += 1;
            self.edge(block, target, None)?;
        }
        Ok(())
    }

    // ---- the end ---------------------------------------------------------------------------

    /// Branch fields filled now that every block and stub is placed.
    fn finish(&mut self) -> Result<(), TierError> {
        let mut k = 0;
        while k < self.patches.len() {
            let patch = self.patches[k];
            k += 1;
            let to = match patch.to & LABEL {
                0 => self.block_at.get(patch.to as usize).copied().unwrap_or(NONE),
                _ => self.labels.get((patch.to & !LABEL) as usize).copied().unwrap_or(NONE),
            };
            if to == NONE {
                return Err(refused_fn("a branch to a block the analysis's order does not place"));
            }
            let FieldKind::Branch { scale, from } = patch.field.kind else {
                return Err(bad_field(u32::from(patch.field.at), "a branch patch on a field that is not a branch"));
            };
            let disp = i64::from(to) - (i64::from(patch.start) + i64::from(from));
            if disp & ((1i64 << scale) - 1) != 0 {
                return Err(bad_field(u32::from(patch.field.at), "a branch displacement not a multiple of its unit"));
            }
            let units = disp >> scale;
            let reach = patch.reach.clamp(1, 63);
            if units < -(1i64 << (reach - 1)) || units >= 1i64 << (reach - 1) {
                return Err(too_large("a branch past its field's reach"));
            }
            self.set_bits(patch.start, patch.len as usize, &patch.field, units as u64)?;
        }
        // The function's own data after the instructions: nothing patched above reaches it.
        self.place_rows()
    }
}

use blaze_tier::LoopHeader;
