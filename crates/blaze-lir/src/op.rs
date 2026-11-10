//! LIR's operations ([`Op`]) and what each takes ([`Op::info`]).
//!
//! An instruction's operands are its one `data` word or, where they do not fit, words of
//! `extra` from the index `data` holds ([`Layout`]). Each operand is a [`Field`], so the
//! verifier, the printer and the parser read every operation from this one table, and a
//! consumer reads an instruction's operands in order with [`Operands`].
//!
//! The vocabulary started as the Cranelift opcodes the builder used, with CLIF's "every address
//! is an integer" replaced by pointer operations and what CLIF lacks added: target intrinsics,
//! volatile and ordered atomics, `memcpy` and kin as operations, lifetime markers, scalable
//! vectors, `assume` and `freeze`.

use crate::{
    flags::{AtomicRmw, FlagClass, FloatCond, IntCond},
    func::{AsmId, Block, IntrinsicId, RefId, SigId, SlotId, Value, View},
    ty::Ty,
};

/// One operand of an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Field {
    /// A value: one word.
    Value,
    /// An unsigned immediate: a lane, a trap code, a member of a compound. One word.
    Imm,
    /// A signed byte offset (a load's or store's): one word, an `i32`.
    Offset,
    /// An [`IntCond`]: one word.
    IntCond,
    /// A [`FloatCond`]: one word.
    FloatCond,
    /// An [`AtomicRmw`]: one word.
    Rmw,
    /// A row of `refs`: one word.
    Ref,
    /// A row of `sigs`: one word.
    Sig,
    /// A row of `slots`: one word.
    Slot,
    /// A row of `intrinsics`: one word.
    Intrinsic,
    /// A row of `asms`: one word.
    Asm,
    /// A block, with no arguments (an unwind edge, an `invoke`'s normal edge): one word.
    Block,
    /// A block and its arguments: the block, their count, then each.
    Target,
    /// A list of values: its count, then each.
    Values,
    /// A list of blocks: its count, then each.
    Blocks,
    /// A shuffle's lanes: its count, then each lane index.
    Mask,
    /// A constant of the instruction's result type: [`Ty::const_words`] words, little-endian.
    Const,
    /// A `switch`'s cases: their count, then each case's value (as many words as the
    /// scrutinee's [`Ty::const_words`]) and its target.
    Cases,
}

/// Where an instruction's operands are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Layout {
    /// None: `data` is 0.
    None,
    /// One operand, in `data`.
    Data(Field),
    /// These operands, in `extra` from `data`.
    Extra(&'static [Field]),
}

impl Layout {
    /// The operands, in order.
    pub const fn fields(&self) -> &'static [Field] {
        match self {
            Layout::None => &[],
            Layout::Data(field) => match field {
                Field::Value => &[Field::Value],
                Field::Imm => &[Field::Imm],
                Field::Ref => &[Field::Ref],
                Field::Slot => &[Field::Slot],
                Field::Offset
                | Field::IntCond
                | Field::FloatCond
                | Field::Rmw
                | Field::Sig
                | Field::Intrinsic
                | Field::Asm
                | Field::Block
                | Field::Target
                | Field::Values
                | Field::Blocks
                | Field::Mask
                | Field::Const
                | Field::Cases => &[],
            },
            Layout::Extra(fields) => fields,
        }
    }
}

/// What an instruction's result is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Res {
    /// No result: its type is [`Ty::NONE`].
    None,
    /// One result, of the type in `tys`.
    Some,
    /// What its signature or `asm!` block returns: none, one, or a compound of several.
    Any,
}

/// Whether an instruction ends its block.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum End {
    No,
    Yes,
}

/// Whether an instruction's `attrs` word may name a row of `mem_attrs`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AttrUse {
    None,
    Mem,
}

/// What an operation takes and makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Info {
    /// Its name in the text form.
    pub name: &'static str,
    pub layout: Layout,
    pub result: Res,
    /// Which flags it may carry.
    pub flags: FlagClass,
    pub attrs: AttrUse,
    pub end: End,
}

const NONE: Layout = Layout::None;
const V: Layout = Layout::Data(Field::Value);
const IMM: Layout = Layout::Data(Field::Imm);
const REF: Layout = Layout::Data(Field::Ref);
const SLOT: Layout = Layout::Data(Field::Slot);
const VV: Layout = Layout::Extra(&[Field::Value, Field::Value]);
const VVV: Layout = Layout::Extra(&[Field::Value, Field::Value, Field::Value]);
const CONST: Layout = Layout::Extra(&[Field::Const]);

macro_rules! ops {
    ($($(#[$meta:meta])* $op:ident = $name:literal: $layout:expr, $res:ident, $class:ident, $attrs:ident, $end:ident;)*) => {
        /// A LIR operation: the byte of the `ops` column.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        #[repr(u8)]
        pub enum Op {
            $( $(#[$meta])* $op, )*
        }

        impl Op {
            /// Every operation, by its byte.
            pub const ALL: &'static [Op] = &[$( Op::$op, )*];
            pub const COUNT: usize = Op::ALL.len();
        }

        /// [`Op::info`] of every operation, by its byte.
        pub const INFOS: [Info; Op::COUNT] = [$(
            Info {
                name: $name,
                layout: $layout,
                result: Res::$res,
                flags: FlagClass::$class,
                attrs: AttrUse::$attrs,
                end: End::$end,
            },
        )*];
    };
}

static INFO: [Info; Op::COUNT] = INFOS;

impl Op {
    /// What the operation takes and makes.
    pub fn info(self) -> &'static Info {
        &INFO[self as usize]
    }

    pub fn name(self) -> &'static str {
        self.info().name
    }

    pub fn from_byte(byte: u8) -> Option<Op> {
        Op::ALL.get(byte as usize).copied()
    }

    pub fn from_name(name: &str) -> Option<Op> {
        Op::ALL.iter().copied().find(|op| op.name() == name)
    }

    /// Whether the operation ends its block.
    pub fn is_terminator(self) -> bool {
        matches!(self.info().end, End::Yes)
    }
}

ops! {
    // Values.
    /// A block parameter: only among the block's leading instructions.
    Param = "param": NONE, Some, None, None, No;
    /// An integer constant in `data`: of a type of at most 32 bits, its bits (zero above the
    /// width); of `i64` or `i128`, a value that fits an `i32`, sign-extended.
    Iconst = "iconst": IMM, Some, None, None, No;
    /// An `i64` or `i128` constant that does not fit an `i32`.
    IconstWide = "iconst_wide": CONST, Some, None, None, No;
    /// An `f16` or `f32` constant: its bits in `data`.
    Fconst = "fconst": IMM, Some, None, None, No;
    /// An `f64` or `f128` constant: its bits.
    FconstWide = "fconst_wide": CONST, Some, None, None, No;
    /// A fixed vector constant: its bytes.
    Vconst = "vconst": CONST, Some, None, None, No;
    /// The null pointer.
    Null = "null": NONE, Some, None, None, No;
    /// A value whose bits are unspecified (`MaybeUninit::uninit()` as a scalar). Every use of
    /// it may see other bits; `freeze` fixes them.
    Uninit = "uninit": NONE, Some, None, None, No;

    // Integers, scalar or lane by lane.
    Add = "add": VV, Some, Wrap, None, No;
    Sub = "sub": VV, Some, Wrap, None, No;
    Mul = "mul": VV, Some, Wrap, None, No;
    Neg = "neg": V, Some, Neg, None, No;
    /// Traps on a zero divisor.
    Udiv = "udiv": VV, Some, Div, None, No;
    /// Traps on a zero divisor and on `MIN / -1`.
    Sdiv = "sdiv": VV, Some, Div, None, No;
    Urem = "urem": VV, Some, Rem, None, No;
    Srem = "srem": VV, Some, Rem, None, No;
    And = "and": VV, Some, None, None, No;
    Or = "or": VV, Some, Or, None, No;
    Xor = "xor": VV, Some, None, None, No;
    Not = "not": V, Some, None, None, No;
    /// Masks its amount to the width, unless `in_range`.
    Shl = "shl": VV, Some, Shl, None, No;
    Lshr = "lshr": VV, Some, Shr, None, No;
    Ashr = "ashr": VV, Some, Shr, None, No;
    Rotl = "rotl": VV, Some, None, None, No;
    Rotr = "rotr": VV, Some, None, None, No;
    Umulhi = "umulhi": VV, Some, None, None, No;
    Smulhi = "smulhi": VV, Some, None, None, No;
    UaddSat = "uadd_sat": VV, Some, None, None, No;
    SaddSat = "sadd_sat": VV, Some, None, None, No;
    UsubSat = "usub_sat": VV, Some, None, None, No;
    SsubSat = "ssub_sat": VV, Some, None, None, No;
    Umin = "umin": VV, Some, None, None, No;
    Umax = "umax": VV, Some, None, None, No;
    Smin = "smin": VV, Some, None, None, No;
    Smax = "smax": VV, Some, None, None, No;
    /// `{T, i1}`: the wrapped result and whether it overflowed.
    UaddOverflow = "uadd_overflow": VV, Some, None, None, No;
    SaddOverflow = "sadd_overflow": VV, Some, None, None, No;
    UsubOverflow = "usub_overflow": VV, Some, None, None, No;
    SsubOverflow = "ssub_overflow": VV, Some, None, None, No;
    UmulOverflow = "umul_overflow": VV, Some, None, None, No;
    SmulOverflow = "smul_overflow": VV, Some, None, None, No;
    /// The width for 0, unless `nonzero`.
    Clz = "clz": V, Some, Count, None, No;
    Ctz = "ctz": V, Some, Count, None, No;
    Popcnt = "popcnt": V, Some, None, None, No;
    Bswap = "bswap": V, Some, None, None, No;
    Bitrev = "bitrev": V, Some, None, None, No;
    /// `i1` (or `<n x i1>`) of the condition, then the operands.
    Icmp = "icmp": Layout::Extra(&[Field::IntCond, Field::Value, Field::Value]), Some, None, None, No;
    Fcmp = "fcmp": Layout::Extra(&[Field::FloatCond, Field::Value, Field::Value]), Some, None, None, No;
    /// The condition (`i1`, or `<n x i1>` lane by lane), then the values if true and false.
    Select = "select": VVV, Some, Select, None, No;

    // Conversions: the result type is the instruction's.
    Zext = "zext": V, Some, None, None, No;
    Sext = "sext": V, Some, None, None, No;
    Trunc = "trunc": V, Some, None, None, No;
    Fpext = "fpext": V, Some, None, None, No;
    Fptrunc = "fptrunc": V, Some, None, None, No;
    /// Saturates (Rust's `as`), unless `in_range` (`to_int_unchecked`). NaN is 0.
    FptoUi = "fptoui": V, Some, Cvt, None, No;
    FptoSi = "fptosi": V, Some, Cvt, None, No;
    UitoFp = "uitofp": V, Some, None, None, No;
    SitoFp = "sitofp": V, Some, None, None, No;
    /// The same bits as another type of the same width: integers, floats and vectors, never
    /// a pointer (`ptr_addr`, `from_exposed`).
    Bitcast = "bitcast": V, Some, None, None, No;

    // Floats, scalar or lane by lane.
    Fadd = "fadd": VV, Some, Float, None, No;
    Fsub = "fsub": VV, Some, Float, None, No;
    Fmul = "fmul": VV, Some, Float, None, No;
    Fdiv = "fdiv": VV, Some, Float, None, No;
    Frem = "frem": VV, Some, Float, None, No;
    /// IEEE 754-2008 `minNum`: a NaN operand gives the other (`f32::min`).
    FminNum = "fmin_num": VV, Some, Float, None, No;
    FmaxNum = "fmax_num": VV, Some, Float, None, No;
    /// IEEE 754-2019 `minimum`: a NaN operand gives NaN (`f32::minimum`).
    Fminimum = "fminimum": VV, Some, Float, None, No;
    Fmaximum = "fmaximum": VV, Some, Float, None, No;
    Fcopysign = "fcopysign": VV, Some, Float, None, No;
    Fneg = "fneg": V, Some, Float, None, No;
    Fabs = "fabs": V, Some, Float, None, No;
    Sqrt = "sqrt": V, Some, Float, None, No;
    Floor = "floor": V, Some, Float, None, No;
    Ceil = "ceil": V, Some, Float, None, No;
    /// Toward zero.
    Ftrunc = "ftrunc": V, Some, Float, None, No;
    /// To the nearest, ties to even.
    Nearest = "nearest": V, Some, Float, None, No;
    /// To the nearest, ties away from zero (`f32::round`).
    Fround = "fround": V, Some, Float, None, No;
    /// Fused: `a * b + c` rounded once.
    Fma = "fma": VVV, Some, Float, None, No;
    /// `a * b + c`, fused or not as the consumer finds faster (`fmuladdf32`).
    Fmuladd = "fmuladd": VVV, Some, Float, None, No;

    // Facts as instructions.
    /// The same value, with any unspecified bits fixed.
    Freeze = "freeze": V, Some, None, None, No;
    /// The `i1` is true (`assume`, `assert_unchecked`): a promise, droppable.
    Assume = "assume": V, None, None, None, No;

    // Pointers.
    /// A pointer plus a byte offset of any integer type, signed.
    PtrAdd = "ptr_add": VV, Some, PtrAdd, None, No;
    /// The address, taking no provenance (`addr`): an integer of the pointer's width.
    PtrAddr = "ptr_addr": V, Some, None, None, No;
    /// The address, exposing the provenance (`expose_provenance`).
    Expose = "expose": V, Some, None, None, No;
    /// A pointer of an exposed provenance (`with_exposed_provenance`).
    FromExposed = "from_exposed": V, Some, None, None, No;
    /// The pointer with another address, keeping its provenance (`with_addr`).
    PtrWithAddr = "ptr_with_addr": VV, Some, None, None, No;
    FuncAddr = "func_addr": REF, Some, None, None, No;
    /// The address of a static or of the function's own data.
    DataAddr = "data_addr": REF, Some, None, None, No;
    /// The address of this thread's instance of a thread-local.
    TlsAddr = "tls_addr": REF, Some, None, None, No;
    SlotAddr = "slot_addr": SLOT, Some, None, None, No;
    /// `StorageLive`: the slot's bytes are uninitialized from here.
    SlotLive = "slot_live": SLOT, None, None, None, No;
    /// `StorageDead`: the slot is not read again until a `slot_live`.
    SlotDead = "slot_dead": SLOT, None, None, None, No;

    // Memory.
    /// The address, then a byte offset from it.
    Load = "load": Layout::Extra(&[Field::Value, Field::Offset]), Some, Mem, Mem, No;
    /// The value, the address, then a byte offset from it.
    Store = "store": Layout::Extra(&[Field::Value, Field::Value, Field::Offset]), None, Mem, Mem, No;
    /// The destination, the source and the length.
    Memcpy = "memcpy": VVV, None, Bulk, Mem, No;
    Memmove = "memmove": VVV, None, Bulk, Mem, No;
    /// The destination, the byte (`i8`) and the length.
    Memset = "memset": VVV, None, Bulk, Mem, No;
    /// The two addresses and the length: an `i32` below, equal to or above 0.
    Memcmp = "memcmp": VVV, Some, None, Mem, No;
    AtomicLoad = "atomic_load": V, Some, Atomic, Mem, No;
    /// The value, then the address.
    AtomicStore = "atomic_store": VV, None, Atomic, Mem, No;
    /// The operation, the address and the operand: the value before.
    AtomicRmw = "atomic_rmw": Layout::Extra(&[Field::Rmw, Field::Value, Field::Value]), Some, Atomic, Mem, No;
    /// The address, the expected and the new value: `{T, i1}`, the value before and whether
    /// it was the expected one.
    Cmpxchg = "cmpxchg": VVV, Some, Cmpxchg, Mem, No;
    Fence = "fence": NONE, None, Fence, None, No;

    // Vectors.
    Splat = "splat": V, Some, None, None, No;
    /// The vector, then the lane.
    Extract = "extract": Layout::Extra(&[Field::Value, Field::Imm]), Some, None, None, No;
    /// The vector, the value, then the lane.
    Insert = "insert": Layout::Extra(&[Field::Value, Field::Value, Field::Imm]), Some, None, None, No;
    /// Two vectors and the lanes of their concatenation to take.
    Shuffle = "shuffle": Layout::Extra(&[Field::Value, Field::Value, Field::Mask]), Some, None, None, No;
    /// The lanes of the first vector the second's lanes index (a table lookup); 0 for one out
    /// of range.
    Swizzle = "swizzle": VV, Some, None, None, No;
    ReduceAdd = "reduce_add": V, Some, None, None, No;
    ReduceMul = "reduce_mul": V, Some, None, None, No;
    ReduceAnd = "reduce_and": V, Some, None, None, No;
    ReduceOr = "reduce_or": V, Some, None, None, No;
    ReduceXor = "reduce_xor": V, Some, None, None, No;
    ReduceUmin = "reduce_umin": V, Some, None, None, No;
    ReduceUmax = "reduce_umax": V, Some, None, None, No;
    ReduceSmin = "reduce_smin": V, Some, None, None, No;
    ReduceSmax = "reduce_smax": V, Some, None, None, No;
    /// The start, then the vector: in lane order, unless `reassoc`.
    ReduceFadd = "reduce_fadd": VV, Some, Float, None, No;
    ReduceFmul = "reduce_fmul": VV, Some, Float, None, No;
    ReduceFmin = "reduce_fmin": V, Some, Float, None, No;
    ReduceFmax = "reduce_fmax": V, Some, Float, None, No;
    /// An integer of each `<n x i1>` lane's bit, lane 0 lowest.
    Bitmask = "bitmask": V, Some, None, None, No;
    /// The address, the mask and the lanes where it is false.
    MaskedLoad = "masked_load": VVV, Some, None, Mem, No;
    /// The value, the address and the mask.
    MaskedStore = "masked_store": VVV, None, None, Mem, No;
    /// The addresses (`<n x ptr>`), the mask and the lanes where it is false.
    Gather = "gather": VVV, Some, None, Mem, No;
    /// The values, the addresses and the mask.
    Scatter = "scatter": VVV, None, None, Mem, No;

    // Calls.
    /// The callee, its signature and the arguments. Unwinds to no handler: a call that may
    /// unwind into one is an `invoke`.
    Call = "call": Layout::Extra(&[Field::Ref, Field::Sig, Field::Values]), Any, Call, None, No;
    CallIndirect = "call_indirect": Layout::Extra(&[Field::Value, Field::Sig, Field::Values]), Any, Call, None, No;
    /// A `core::arch` intrinsic and its arguments.
    TargetIntrinsic = "target_intrinsic": Layout::Extra(&[Field::Intrinsic, Field::Values]), Any, None, None, No;
    /// An `asm!` block and its inputs, in operand order: its outputs, in operand order.
    Asm = "asm": Layout::Extra(&[Field::Asm, Field::Values]), Any, None, None, No;
    /// Member `k` of a compound value.
    Project = "project": Layout::Extra(&[Field::Value, Field::Imm]), Some, None, None, No;
    FrameAddress = "frame_address": NONE, Some, None, None, No;
    ReturnAddress = "return_address": NONE, Some, None, None, No;
    StackPointer = "stack_pointer": NONE, Some, None, None, No;

    // Ends of blocks.
    Jump = "jump": Layout::Extra(&[Field::Target]), None, None, None, Yes;
    /// The `i1` condition, the target if true, then if false.
    Br = "br": Layout::Extra(&[Field::Value, Field::Target, Field::Target]), None, Br, None, Yes;
    /// The integer scrutinee, the default target, then the cases (MIR's `SwitchInt`).
    Switch = "switch": Layout::Extra(&[Field::Value, Field::Target, Field::Cases]), None, None, None, Yes;
    /// The function's returns, as its signature has them.
    Return = "return": Layout::Extra(&[Field::Values]), None, None, None, Yes;
    Unreachable = "unreachable": NONE, None, None, None, Yes;
    /// Traps with the code in `data`.
    Trap = "trap": IMM, None, None, None, Yes;
    /// `UnwindResume`: goes on unwinding with the exception.
    Resume = "resume": V, None, None, None, Yes;
    /// A call that may unwind: the callee, its signature, where it returns to, where it
    /// unwinds to (a landing block), then the arguments. Its result is a value of the block it
    /// returns to, whose one predecessor it is.
    Invoke = "invoke": Layout::Extra(&[Field::Ref, Field::Sig, Field::Block, Field::Block, Field::Values]), Any, Call, None, Yes;
    InvokeIndirect = "invoke_indirect": Layout::Extra(&[Field::Value, Field::Sig, Field::Block, Field::Block, Field::Values]), Any, Call, None, Yes;
    /// `become`: the callee, its signature and the arguments.
    TailCall = "tail_call": Layout::Extra(&[Field::Ref, Field::Sig, Field::Values]), None, Call, None, Yes;
    TailCallIndirect = "tail_call_indirect": Layout::Extra(&[Field::Value, Field::Sig, Field::Values]), None, Call, None, Yes;
    /// An `asm!` block with `label` operands: the block, where it falls through to, its labels,
    /// then its inputs. Its outputs are a value of the block it falls through to, whose one
    /// predecessor it is.
    AsmBr = "asm_br": Layout::Extra(&[Field::Asm, Field::Block, Field::Blocks, Field::Values]), Any, None, None, Yes;
}

/// A code of the operation table: its name, layout, result, flags, attributes and end, so a
/// change to any is a change of the record's schema ([`crate::persist::SCHEMA`]).
pub const fn table_code() -> [u64; Op::COUNT] {
    let mut codes = [0; Op::COUNT];
    let mut i = 0;
    while i < Op::COUNT {
        let info = &INFOS[i];
        let fields = info.layout.fields();
        let mut code: u64 = match info.layout {
            Layout::None => 1,
            Layout::Data(_) => 2,
            Layout::Extra(_) => 3,
        };
        let mut f = 0;
        while f < fields.len() {
            code = code.wrapping_mul(37).wrapping_add(fields[f] as u64 + 1);
            f += 1;
        }
        code = code.wrapping_mul(37).wrapping_add(info.result as u64);
        code = code.wrapping_mul(37).wrapping_add(info.flags as u64);
        code = code.wrapping_mul(37).wrapping_add(info.attrs as u64);
        code = code.wrapping_mul(37).wrapping_add(info.end as u64);
        let name = info.name.as_bytes();
        let mut c = 0;
        while c < name.len() {
            code = code.wrapping_mul(131).wrapping_add(name[c] as u64);
            c += 1;
        }
        codes[i] = code;
        i += 1;
    }
    codes
}

/// A list of values in `extra`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Values<'a>(pub &'a [u32]);

impl<'a> Values<'a> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, at: usize) -> Value {
        Value(self.0[at])
    }

    pub fn iter(self) -> impl ExactSizeIterator<Item = Value> + 'a {
        self.0.iter().map(|&it| Value(it))
    }
}

/// A block and the arguments its parameters take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target<'a> {
    pub block: Block,
    pub args: Values<'a>,
}

/// A `switch`'s cases, in order: each value's words and its target.
#[derive(Clone, Copy, Debug)]
pub struct Cases<'a> {
    words: &'a [u32],
    width: usize,
    left: u32,
}

impl<'a> Iterator for Cases<'a> {
    type Item = (&'a [u32], Target<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let (value, rest) = self.words.split_at(self.width);
        let block = Block(rest[0]);
        let n = rest[1] as usize;
        let args = Values(&rest[2..2 + n]);
        self.words = &rest[2 + n..];
        Some((value, Target { block, args }))
    }
}

/// An instruction's operands, read in order. Each read panics past the payload, which a built
/// or read function's never is: the verifier's structure check holds every payload in bounds.
#[derive(Clone, Copy, Debug)]
pub struct Operands<'a> {
    words: &'a [u32],
    at: usize,
    tys: &'a [u16],
    ty: Ty,
}

impl<'a> Operands<'a> {
    pub(crate) fn of(view: View<'a>, inst: usize) -> Operands<'a> {
        let words: &'a [u32] = match view.op(inst).info().layout {
            Layout::None => &[],
            Layout::Data(_) => std::slice::from_ref(&view.data[inst]),
            Layout::Extra(_) => &view.extra[view.data[inst] as usize..],
        };
        Operands { words, at: 0, tys: view.tys, ty: Ty(view.tys[inst]) }
    }

    /// How many words have been read.
    pub fn taken(&self) -> usize {
        self.at
    }

    pub fn word(&mut self) -> u32 {
        let word = self.words[self.at];
        self.at += 1;
        word
    }

    fn list(&mut self) -> &'a [u32] {
        let n = self.word() as usize;
        let list = &self.words[self.at..self.at + n];
        self.at += n;
        list
    }

    pub fn value(&mut self) -> Value {
        Value(self.word())
    }

    pub fn imm(&mut self) -> u32 {
        self.word()
    }

    pub fn offset(&mut self) -> i32 {
        self.word() as i32
    }

    pub fn int_cond(&mut self) -> IntCond {
        IntCond::from_word(self.word()).expect("a condition of a checked function")
    }

    pub fn float_cond(&mut self) -> FloatCond {
        FloatCond::from_word(self.word()).expect("a condition of a checked function")
    }

    pub fn rmw(&mut self) -> AtomicRmw {
        AtomicRmw::from_word(self.word()).expect("an operation of a checked function")
    }

    pub fn reference(&mut self) -> RefId {
        RefId(self.word())
    }

    pub fn sig(&mut self) -> SigId {
        SigId(self.word())
    }

    pub fn slot(&mut self) -> SlotId {
        SlotId(self.word())
    }

    pub fn intrinsic(&mut self) -> IntrinsicId {
        IntrinsicId(self.word())
    }

    pub fn asm(&mut self) -> AsmId {
        AsmId(self.word())
    }

    pub fn block(&mut self) -> Block {
        Block(self.word())
    }

    pub fn target(&mut self) -> Target<'a> {
        let block = self.block();
        Target { block, args: Values(self.list()) }
    }

    pub fn values(&mut self) -> Values<'a> {
        Values(self.list())
    }

    pub fn blocks(&mut self) -> &'a [u32] {
        self.list()
    }

    pub fn mask(&mut self) -> &'a [u32] {
        self.list()
    }

    /// A constant of the instruction's result type, its words little-endian.
    pub fn constant(&mut self) -> &'a [u32] {
        let n = self.ty.const_words() as usize;
        let words = &self.words[self.at..self.at + n];
        self.at += n;
        words
    }

    /// A `switch`'s cases, of the scrutinee `scrutinee` (its first operand).
    pub fn cases(&mut self, scrutinee: Value) -> Cases<'a> {
        let width = Ty(self.tys[scrutinee.index()]).const_words() as usize;
        let left = self.word();
        let start = self.at;
        for _ in 0..left {
            self.at += width;
            self.at += 1;
            let n = self.word() as usize;
            self.at += n;
        }
        Cases { words: &self.words[start..self.at], width, left }
    }
}

/// One word of an operand, as [`walk`] hands it on: what it names and the word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Word {
    /// A value (an operand, an argument).
    Value(u32),
    /// A block (a target, an edge, a label).
    Block(u32),
    /// A word of another field: an immediate, an enum, a row of a table, a mask's lane, a
    /// constant's or a case value's word.
    Of(Field, u32),
}

/// Walks the operands `fields` from `words`, checking each list and constant lies in
/// `words`, handing `visit` every word with what it is. Returns how many words they take,
/// or why they do not fit. `ty` is the instruction's result type and `tys` the function's
/// type column (a `switch`'s case width is its scrutinee's, the value before).
pub fn walk<E>(
    words: &[u32],
    fields: &[Field],
    ty: Ty,
    tys: &[u16],
    visit: &mut impl FnMut(Word) -> Result<(), E>,
) -> Result<usize, WalkError<E>> {
    let mut at = 0;
    let mut last_value = None;
    let take = |at: &mut usize| -> Result<u32, WalkError<E>> {
        let word = *words.get(*at).ok_or(WalkError::Short)?;
        *at += 1;
        Ok(word)
    };
    for &field in fields {
        match field {
            Field::Value => {
                let word = take(&mut at)?;
                last_value = Some(word);
                visit(Word::Value(word)).map_err(WalkError::Visit)?;
            }
            Field::Block => {
                let word = take(&mut at)?;
                visit(Word::Block(word)).map_err(WalkError::Visit)?;
            }
            Field::Imm
            | Field::Offset
            | Field::IntCond
            | Field::FloatCond
            | Field::Rmw
            | Field::Ref
            | Field::Sig
            | Field::Slot
            | Field::Intrinsic
            | Field::Asm => {
                let word = take(&mut at)?;
                visit(Word::Of(field, word)).map_err(WalkError::Visit)?;
            }
            Field::Target => walk_target(words, &mut at, visit)?,
            Field::Values => walk_list(words, &mut at, visit, Word::Value)?,
            Field::Blocks => walk_list(words, &mut at, visit, Word::Block)?,
            Field::Mask => walk_list(words, &mut at, visit, |it| Word::Of(Field::Mask, it))?,
            Field::Const => {
                for _ in 0..ty.const_words() {
                    let word = take(&mut at)?;
                    visit(Word::Of(Field::Const, word)).map_err(WalkError::Visit)?;
                }
            }
            Field::Cases => {
                let scrutinee = last_value.ok_or(WalkError::NoScrutinee)?;
                let ty = Ty(*tys.get(scrutinee as usize).ok_or(WalkError::NoScrutinee)?);
                let n = take(&mut at)? as usize;
                if n > words.len() - at {
                    return Err(WalkError::Short);
                }
                for _ in 0..n {
                    for _ in 0..ty.const_words() {
                        let word = take(&mut at)?;
                        visit(Word::Of(Field::Cases, word)).map_err(WalkError::Visit)?;
                    }
                    walk_target(words, &mut at, visit)?;
                }
            }
        }
    }
    Ok(at)
}

fn walk_list<E>(
    words: &[u32],
    at: &mut usize,
    visit: &mut impl FnMut(Word) -> Result<(), E>,
    make: impl Fn(u32) -> Word,
) -> Result<(), WalkError<E>> {
    let n = *words.get(*at).ok_or(WalkError::Short)? as usize;
    *at += 1;
    if n > words.len() - *at {
        return Err(WalkError::Short);
    }
    for _ in 0..n {
        let word = words[*at];
        *at += 1;
        visit(make(word)).map_err(WalkError::Visit)?;
    }
    Ok(())
}

fn walk_target<E>(
    words: &[u32],
    at: &mut usize,
    visit: &mut impl FnMut(Word) -> Result<(), E>,
) -> Result<(), WalkError<E>> {
    let block = *words.get(*at).ok_or(WalkError::Short)?;
    *at += 1;
    visit(Word::Block(block)).map_err(WalkError::Visit)?;
    let n = *words.get(*at).ok_or(WalkError::Short)? as usize;
    *at += 1;
    if n > words.len() - *at {
        return Err(WalkError::Short);
    }
    for _ in 0..n {
        let word = words[*at];
        *at += 1;
        visit(Word::Value(word)).map_err(WalkError::Visit)?;
    }
    Ok(())
}

/// Why [`walk`] stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalkError<E> {
    /// The operands run past their words.
    Short,
    /// A `switch`'s cases with no value before them, or one past the type column.
    NoScrutinee,
    /// What the visitor refused.
    Visit(E),
}

#[cfg(test)]
mod tests {
    use super::{INFOS, Layout, Op};

    #[test]
    fn every_op_is_named_once_and_found_by_its_name() {
        for (byte, &op) in Op::ALL.iter().enumerate() {
            assert_eq!(op as usize, byte);
            assert_eq!(Op::from_name(op.name()), Some(op), "{}", op.name());
            assert!(!op.name().contains('.'), "{} has a dot, which the text form splits flags at", op.name());
        }
        assert!(Op::COUNT <= u8::MAX as usize);
    }

    #[test]
    fn data_layouts_are_one_word() {
        for info in INFOS {
            if let Layout::Data(_) = info.layout {
                assert_eq!(info.layout.fields().len(), 1, "{}", info.name);
            }
        }
    }
}
