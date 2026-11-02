//! Lowers one function of Cranelift IR to a WebAssembly function body.
//!
//! Cranelift has no WebAssembly backend, so this takes the IR the rest of codegen builds and
//! writes wasm from it directly. The target is `wasm32`, so an address is an `i32`.
//!
//! What comes out is not a module. A [`WasmFunction`] names the functions and data it uses
//! through relocations, like machine code does, and a linker gives them places:
//!
//! - a call names the function ([`Calls::Direct`]) or goes through the function's slot in
//!   the function table ([`Calls::Table`]), so that a function is replaced by writing its
//!   slot;
//! - the address of a function is its slot, the address of data is where the linker put it
//!   in linear memory;
//! - stack slots live on the stack in linear memory, whose pointer is the global
//!   `__stack_pointer`.
//!
//! The signatures are the ones LLVM gives the same functions, since the code is linked with
//! rustc's: a function that returns more than one value, or an `i128`, writes them through
//! a pointer it takes ahead of its parameters.
//!
//! Unwinding is not lowered: the exceptional edges of `try_call` are dropped, which is what
//! `panic=abort` means, and the wasm targets rustc has are built that way.
//!
//! A vector of 128 bits is a `v128` (see [`v128`]): code generation makes one only where the
//! function may use the `simd128` feature. A wasm instruction Cranelift has no opcode for is
//! a call of a name of [`INSTRUCTION`], which is lowered to the instruction in place.

#![forbid(unsafe_code)]

mod v128;

pub use v128::{INSTRUCTION, Instruction};

use blaze_codegen::wasm as format;
use cranelift_codegen::{
    entity::{EntityRef, SecondaryMap},
    ir::{
        self, AtomicRmwOp, Block, BlockArg, BlockCall, ExternalName, Function, GlobalValueData,
        InstructionData, LibCall, Opcode, Signature, Type, Value,
        condcodes::{FloatCC, IntCC},
        types,
    },
};
use cranelift_module::{ModuleReloc, ModuleRelocTarget};
use serde_derive::{Deserialize, Serialize};
use smallvec::SmallVec;
use wasm_encoder::{
    BlockType, Encode, Instruction as I, InstructionSink, MemArg, MemArgTarget, ValType,
};

use super::target::Calls;

/// How many bytes an operand a linker fills in takes: a LEB128 padded to full length.
pub const INDEX_LEN: usize = 5;

/// The relocation of a `call` operand: the index of the function. The other relocation
/// here, `Abs4`, is of an `i32.const` operand: the address of data, or the slot of a function.
pub(super) const CALL: cranelift_codegen::binemit::Reloc =
    cranelift_codegen::binemit::Reloc::X86CallPCRel4;

/// A value type, as its byte in the binary format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum WasmType {
    I32 = 0x7f,
    I64 = 0x7e,
    F32 = 0x7d,
    F64 = 0x7c,
    V128 = 0x7b,
}

impl From<WasmType> for ValType {
    fn from(ty: WasmType) -> ValType {
        match ty {
            WasmType::I32 => ValType::I32,
            WasmType::I64 => ValType::I64,
            WasmType::F32 => ValType::F32,
            WasmType::F64 => ValType::F64,
            WasmType::V128 => ValType::V128,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)] // clone: the object keeps each distinct signature once, for its type section
pub struct WasmSignature {
    pub params: Vec<WasmType>,
    pub results: Vec<WasmType>,
}

impl WasmType {
    fn format(self) -> format::ValType {
        match self {
            WasmType::I32 => format::ValType::I32,
            WasmType::I64 => format::ValType::I64,
            WasmType::F32 => format::ValType::F32,
            WasmType::F64 => format::ValType::F64,
            WasmType::V128 => format::ValType::V128,
        }
    }

    fn of_format(ty: format::ValType) -> Result<WasmType, String> {
        Ok(match ty {
            format::ValType::I32 => WasmType::I32,
            format::ValType::I64 => WasmType::I64,
            format::ValType::F32 => WasmType::F32,
            format::ValType::F64 => WasmType::F64,
            format::ValType::V128 => WasmType::V128,
            // alloc: an error
            other => return Err(format!("wasm: a {other:?} in a signature, which Rust code has none of")),
        })
    }
}

impl WasmSignature {
    /// As the linker's format has it.
    pub fn format(&self) -> format::Signature {
        format::Signature {
            // alloc: per signature a function or an import has, a few bytes the format owns
            params: self.params.iter().map(|&it| it.format()).collect(),
            // alloc: as above
            results: self.results.iter().map(|&it| it.format()).collect(),
        }
    }

    fn of_format(signature: &format::Signature) -> Result<WasmSignature, String> {
        let types = |values: &[format::ValType]| {
            values.iter().map(|&it| WasmType::of_format(it)).collect::<Result<Vec<_>, _>>()
        };
        Ok(WasmSignature { params: types(&signature.params)?, results: types(&signature.results)? })
    }

    pub fn of(sig: &Signature) -> Result<WasmSignature, String> {
        let mut params = Vec::with_capacity(sig.params.len() + 1);
        let returned = Returned::of(sig)?;
        if let Returned::Through(_) = returned {
            params.push(WasmType::I32);
        }
        for param in &sig.params {
            match Class::of(param.value_type)? {
                Class::Pair => params.extend([WasmType::I64, WasmType::I64]),
                class => params.push(class.wasm()),
            }
        }
        let results = match returned {
            // alloc: the signature's one result, part of the signature it returns
            Returned::Value(class) => vec![class.wasm()],
            // alloc: empty, allocates nothing
            Returned::Nothing | Returned::Through(_) => Vec::new(),
        };
        Ok(WasmSignature { params, results })
    }
}

/// How a function hands back what it returns.
#[derive(Debug, PartialEq, Eq)]
enum Returned {
    Nothing,
    Value(Class),
    /// Written through a pointer that is the first parameter, each value at its offset: what
    /// LLVM does with a return of several values or of an `i128` where a wasm function has
    /// one result of one of four types.
    Through(SmallVec<[(u64, Class); 8]>),
}

impl Returned {
    /// The bytes a caller sets aside for [`Returned::Through`], and their alignment.
    const SPACE: u64 = 64;
    const ALIGN: u64 = 16;

    fn of(sig: &Signature) -> Result<Returned, String> {
        let classes: SmallVec<[Class; 8]> =
            sig.returns.iter().map(|it| Class::of(it.value_type)).collect::<Result<_, _>>()?;
        match classes[..] {
            [] => return Ok(Returned::Nothing),
            [class] if class != Class::Pair => return Ok(Returned::Value(class)),
            _ => {}
        }
        // The fields of the struct the values are to LLVM, each aligned to its size.
        let mut end = 0u64;
        let mut places = SmallVec::new();
        for class in classes {
            let size = class.bytes();
            let at = end.next_multiple_of(size);
            places.push((at, class));
            end = at + size;
        }
        if end > Returned::SPACE {
            // alloc: the error's message
            return Err("wasm: more returned values than there is room for".to_owned());
        }
        Ok(Returned::Through(places))
    }
}

/// A function body with what a linker has to fill in.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmFunction {
    pub signature: WasmSignature,
    /// The signatures of the indirect calls in the body.
    pub types: Vec<WasmSignature>,
    /// Where the body has a type index to fill in, and which of `types` it is. Each place is
    /// [`INDEX_LEN`] bytes.
    pub type_fixups: Vec<(u32, u32)>,
    /// Where the body has the index of the stack pointer's global to fill in.
    pub stack_pointer_fixups: Vec<u32>,
    /// Local declarations and instructions, as the code section has them after the size.
    pub body: Vec<u8>,
}

impl WasmFunction {
    /// The bytes that stand for the function where machine code would ([`format::Code`]),
    /// and where in them `body` starts: relocations are counted from the start of these.
    pub fn encode(&self) -> Result<(Vec<u8>, u32), String> {
        Ok(self.code().encode(&self.body))
    }

    /// Reads what [`Self::encode`] wrote. The second value is where the body starts in
    /// `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<(WasmFunction, usize), String> {
        let (code, start) = format::Code::decode(bytes)?;
        if !code.imports.is_empty() || code.export.is_some() {
            // alloc: an error
            return Err("wasm: a compiled function's header with imports or an export".to_owned());
        }
        let function = WasmFunction {
            signature: WasmSignature::of_format(&code.ty)?,
            types: code.types.iter().map(WasmSignature::of_format).collect::<Result<_, _>>()?,
            type_fixups: code.type_fixups,
            stack_pointer_fixups: code.stack_pointer,
            // alloc: the readers patch the body in place, on their own copy
            body: bytes[start..].to_vec(),
        };
        Ok((function, start))
    }

    /// The header the linker reads ahead of the body.
    pub fn code(&self) -> format::Code {
        format::Code {
            ty: self.signature.format(),
            // alloc: once a compiled function, its header: a few words beside the body
            types: self.types.iter().map(WasmSignature::format).collect(),
            // clone: as above
            type_fixups: self.type_fixups.clone(),
            // clone: as above
            stack_pointer: self.stack_pointer_fixups.clone(),
            // alloc: empty, allocates nothing
            imports: Vec::new(),
            export: None,
        }
    }
}

/// Lowers `func`. The relocations are relative to the start of the body.
pub(super) fn lower(
    func: &Function,
    calls: Calls,
) -> Result<(WasmFunction, Vec<ModuleReloc>), String> {
    Lower::new(func, calls)?.run()
}

/// How a Cranelift type is held in wasm locals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// `i8`, `i16` and `i32` in an `i32`, the narrow ones zero-extended.
    Word(u8),
    Long,
    /// `i128` in two `i64`, low half first.
    Pair,
    F32,
    F64,
    /// Any vector of 128 bits, whatever its lanes.
    V128,
}

impl Class {
    fn of(ty: Type) -> Result<Class, String> {
        Ok(match ty {
            types::I8 => Class::Word(8),
            types::I16 => Class::Word(16),
            types::I32 => Class::Word(32),
            types::I64 => Class::Long,
            types::I128 => Class::Pair,
            types::F32 => Class::F32,
            types::F64 => Class::F64,
            _ if ty.is_vector() && ty.bits() == 128 => Class::V128,
            // alloc: the error's message
            _ => return Err(format!("wasm: type {ty}")),
        })
    }

    fn wasm(self) -> WasmType {
        match self {
            Class::Word(_) => WasmType::I32,
            Class::Long | Class::Pair => WasmType::I64,
            Class::F32 => WasmType::F32,
            Class::F64 => WasmType::F64,
            Class::V128 => WasmType::V128,
        }
    }

    fn narrow(self) -> Option<u8> {
        match self {
            Class::Word(bits @ (8 | 16)) => Some(bits),
            _ => None,
        }
    }

    fn is_long(self) -> bool {
        self == Class::Long
    }

    /// How many bytes the value is in memory.
    fn bytes(self) -> u64 {
        match self {
            Class::Word(bits) => u64::from(bits / 8),
            Class::F32 => 4,
            Class::Long | Class::F64 => 8,
            Class::Pair | Class::V128 => 16,
        }
    }
}

/// An integer operation that `i32` and `i64` both have.
#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    ShrU,
    ShrS,
    Rotl,
    Rotr,
    DivU,
    DivS,
    RemU,
    RemS,
    Eq,
    Ne,
    LtU,
    LtS,
    GtU,
    GtS,
    LeU,
    LeS,
    GeU,
    GeS,
    Popcnt,
}

impl Op {
    fn compare(cond: IntCC) -> Op {
        match cond {
            IntCC::Equal => Op::Eq,
            IntCC::NotEqual => Op::Ne,
            IntCC::SignedLessThan => Op::LtS,
            IntCC::SignedGreaterThanOrEqual => Op::GeS,
            IntCC::SignedGreaterThan => Op::GtS,
            IntCC::SignedLessThanOrEqual => Op::LeS,
            IntCC::UnsignedLessThan => Op::LtU,
            IntCC::UnsignedGreaterThanOrEqual => Op::GeU,
            IntCC::UnsignedGreaterThan => Op::GtU,
            IntCC::UnsignedLessThanOrEqual => Op::LeU,
        }
    }

    fn is_signed(self) -> bool {
        matches!(self, Op::ShrS | Op::DivS | Op::RemS | Op::LtS | Op::GtS | Op::LeS | Op::GeS)
    }

    /// The comparison without its `or equal`, and unsigned: what decides between two wide
    /// integers whose high halves are equal.
    fn strict_unsigned(self) -> Op {
        match self {
            Op::LtS | Op::LtU | Op::LeS | Op::LeU => Op::LtU,
            _ => Op::GtU,
        }
    }

    fn strict(self) -> Op {
        match self {
            Op::LeS => Op::LtS,
            Op::LeU => Op::LtU,
            Op::GeS => Op::GtS,
            Op::GeU => Op::GtU,
            other => other,
        }
    }

    fn or_equal(self) -> bool {
        matches!(self, Op::LeS | Op::LeU | Op::GeS | Op::GeU)
    }

    fn instruction(self, long: bool) -> I<'static> {
        match (self, long) {
            (Op::Add, false) => I::I32Add,
            (Op::Add, true) => I::I64Add,
            (Op::Sub, false) => I::I32Sub,
            (Op::Sub, true) => I::I64Sub,
            (Op::Mul, false) => I::I32Mul,
            (Op::Mul, true) => I::I64Mul,
            (Op::And, false) => I::I32And,
            (Op::And, true) => I::I64And,
            (Op::Or, false) => I::I32Or,
            (Op::Or, true) => I::I64Or,
            (Op::Xor, false) => I::I32Xor,
            (Op::Xor, true) => I::I64Xor,
            (Op::Shl, false) => I::I32Shl,
            (Op::Shl, true) => I::I64Shl,
            (Op::ShrU, false) => I::I32ShrU,
            (Op::ShrU, true) => I::I64ShrU,
            (Op::ShrS, false) => I::I32ShrS,
            (Op::ShrS, true) => I::I64ShrS,
            (Op::Rotl, false) => I::I32Rotl,
            (Op::Rotl, true) => I::I64Rotl,
            (Op::Rotr, false) => I::I32Rotr,
            (Op::Rotr, true) => I::I64Rotr,
            (Op::DivU, false) => I::I32DivU,
            (Op::DivU, true) => I::I64DivU,
            (Op::DivS, false) => I::I32DivS,
            (Op::DivS, true) => I::I64DivS,
            (Op::RemU, false) => I::I32RemU,
            (Op::RemU, true) => I::I64RemU,
            (Op::RemS, false) => I::I32RemS,
            (Op::RemS, true) => I::I64RemS,
            (Op::Eq, false) => I::I32Eq,
            (Op::Eq, true) => I::I64Eq,
            (Op::Ne, false) => I::I32Ne,
            (Op::Ne, true) => I::I64Ne,
            (Op::LtU, false) => I::I32LtU,
            (Op::LtU, true) => I::I64LtU,
            (Op::LtS, false) => I::I32LtS,
            (Op::LtS, true) => I::I64LtS,
            (Op::GtU, false) => I::I32GtU,
            (Op::GtU, true) => I::I64GtU,
            (Op::GtS, false) => I::I32GtS,
            (Op::GtS, true) => I::I64GtS,
            (Op::LeU, false) => I::I32LeU,
            (Op::LeU, true) => I::I64LeU,
            (Op::LeS, false) => I::I32LeS,
            (Op::LeS, true) => I::I64LeS,
            (Op::GeU, false) => I::I32GeU,
            (Op::GeU, true) => I::I64GeU,
            (Op::GeS, false) => I::I32GeS,
            (Op::GeS, true) => I::I64GeS,
            (Op::Popcnt, false) => I::I32Popcnt,
            (Op::Popcnt, true) => I::I64Popcnt,
        }
    }
}

struct Lower<'a> {
    func: &'a Function,
    calls: Calls,
    signature: WasmSignature,
    returned: Returned,
    /// Where in the frame the values are written that a called function returns through a
    /// pointer.
    returned_at: Option<u64>,
    /// The blocks a jump reaches. The others are where a call unwinds to.
    reachable: SecondaryMap<Block, bool>,
    code: Vec<u8>,
    /// The locals after the parameters.
    locals: Vec<ValType>,
    param_count: u32,
    /// The first local of each value, plus one.
    values: SecondaryMap<Value, u32>,
    /// The place of each block in the layout, plus one.
    order: SecondaryMap<Block, u32>,
    block_count: u32,
    /// The local that says which block runs next, when a block jumps backwards.
    next_block: Option<u32>,
    /// The local that holds the address of the stack slots, and their size together.
    frame: Option<(u32, u64)>,
    slot_offsets: Vec<u64>,
    relocs: Vec<ModuleReloc>,
    types: Vec<WasmSignature>,
    type_fixups: Vec<(u32, u32)>,
    stack_pointer_fixups: Vec<u32>,
}

const MEMORY: u32 = 0;

/// The first local of each value a call returns.
type Returns = SmallVec<[u32; 8]>;

fn mem(offset: u64) -> MemArg {
    MemArg { offset, align: 0, target: MemArgTarget::Memory(MEMORY), ordering: None }
}

impl<'a> Lower<'a> {
    fn new(func: &'a Function, calls: Calls) -> Result<Self, String> {
        let signature = WasmSignature::of(&func.signature)?;
        let returned = Returned::of(&func.signature)?;
        let mut order = SecondaryMap::new();
        let mut block_count = 0;
        for block in func.layout.blocks() {
            block_count += 1;
            order[block] = block_count;
        }
        let mut slot_offsets = Vec::with_capacity(func.sized_stack_slots.len());
        let mut size = 0u64;
        for (_, slot) in func.sized_stack_slots.iter() {
            let align = 1u64 << slot.align_shift;
            size = size.next_multiple_of(align);
            slot_offsets.push(size);
            size += u64::from(slot.size);
        }
        if func.dynamic_stack_slots.iter().next().is_some() {
            // alloc: the error's message
            return Err("wasm: dynamic stack slot".to_owned());
        }
        let mut returned_at = None;
        for (_, called) in func.dfg.signatures.iter() {
            if let Returned::Through(_) = Returned::of(called)? {
                size = size.next_multiple_of(Returned::ALIGN);
                returned_at = Some(size);
                size += Returned::SPACE;
                break;
            }
        }
        let mut this = Lower {
            func,
            calls,
            returned,
            returned_at,
            reachable: Self::reachable(func),
            param_count: signature.params.len() as u32,
            signature,
            // alloc: once per function, the instructions of the body it returns
            code: Vec::new(),
            // alloc: once per function, its locals as the body declares them
            locals: Vec::new(),
            values: SecondaryMap::new(),
            order,
            block_count,
            next_block: None,
            frame: None,
            slot_offsets,
            // alloc: once per function, moved into what it returns; empty until pushed
            relocs: Vec::new(),
            // alloc: as above
            types: Vec::new(),
            // alloc: as above
            type_fixups: Vec::new(),
            // alloc: as above
            stack_pointer_fixups: Vec::new(),
        };
        if size != 0 || !this.slot_offsets.is_empty() {
            let frame = this.temp(ValType::I32);
            this.frame = Some((frame, size.next_multiple_of(16)));
        }
        Ok(this)
    }

    fn run(mut self) -> Result<(WasmFunction, Vec<ModuleReloc>), String> {
        let func = self.func;
        let Some(entry) = func.layout.entry_block() else {
            // alloc: the error's message
            return Err("wasm: function without blocks".to_owned());
        };
        // The parameters of the entry block are the parameters of the function, after the
        // pointer it returns through.
        let mut local = if let Returned::Through(_) = self.returned { 1 } else { 0 };
        for &param in func.dfg.block_params(entry) {
            self.values[param] = local + 1;
            let class = self.class(param)?;
            if let Some(bits) = class.narrow() {
                // Whoever calls from outside may leave anything in the upper bits.
                self.i(I::LocalGet(local));
                self.mask(bits);
                self.i(I::LocalSet(local));
            }
            local += if class == Class::Pair { 2 } else { 1 };
        }
        if local != self.param_count {
            // alloc: the error's message
            return Err("wasm: entry block and signature disagree".to_owned());
        }
        if let Some((frame, size)) = self.frame {
            self.stack_pointer(false);
            self.i(I::I32Const(size as i32));
            self.i(I::I32Sub);
            self.i(I::LocalTee(frame));
            self.stack_pointer(true);
        }

        // Blocks are laid out one after the other, each inside a wasm block that ends where
        // it starts, so a jump forwards is a `br` out of the blocks in between. A jump
        // backwards sets which block is next and goes around a loop that holds all of them,
        // where a `br_table` finds the block. That is right for any control flow graph,
        // reducible or not, and costs only on the edges that go backwards.
        let backwards = func.layout.blocks().any(|block| {
            let from = self.order[block];
            func.layout.last_inst(block).is_some_and(|last| {
                func.dfg.insts[last]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .any(|call| self.order[call.block(&func.dfg.value_lists)] <= from)
            })
        });
        if backwards {
            self.next_block = Some(self.temp(ValType::I32));
            self.i(I::Loop(BlockType::Empty));
        }
        for _ in 0..self.block_count {
            self.i(I::Block(BlockType::Empty));
        }
        if let Some(next) = self.next_block {
            self.i(I::LocalGet(next));
            InstructionSink::new(&mut self.code).br_table(0..self.block_count, 0);
        }
        for block in func.layout.blocks() {
            self.i(I::End);
            if !self.reachable[block] {
                self.i(I::Unreachable);
                continue;
            }
            for inst in func.layout.block_insts(block) {
                self.inst(block, inst)?;
            }
        }
        if backwards {
            self.i(I::End);
        }
        self.i(I::Unreachable);
        self.i(I::End);

        // Runs of the same type, as the format wants them.
        let mut runs: SmallVec<[(u32, ValType); 8]> = SmallVec::new();
        for &ty in &self.locals {
            match runs.last_mut() {
                Some((count, last)) if *last == ty => *count += 1,
                _ => runs.push((1, ty)),
            }
        }
        // A LEB128 of a `u32` is at most five bytes, a value type one.
        let mut body = Vec::with_capacity(5 + runs.len() * 6 + self.code.len());
        (runs.len() as u32).encode(&mut body);
        for (count, ty) in runs {
            count.encode(&mut body);
            ty.encode(&mut body);
        }
        let start = body.len() as u32;
        body.extend(self.code);
        for reloc in &mut self.relocs {
            reloc.offset += start;
        }
        for (offset, _) in &mut self.type_fixups {
            *offset += start;
        }
        for offset in &mut self.stack_pointer_fixups {
            *offset += start;
        }
        let function = WasmFunction {
            signature: self.signature,
            types: self.types,
            type_fixups: self.type_fixups,
            stack_pointer_fixups: self.stack_pointer_fixups,
            body,
        };
        Ok((function, self.relocs))
    }

    /// The blocks reached from the entry without unwinding.
    fn reachable(func: &Function) -> SecondaryMap<Block, bool> {
        let mut reachable = SecondaryMap::new();
        let mut pending: SmallVec<[Block; 16]> = SmallVec::new();
        pending.extend(func.layout.entry_block());
        while let Some(block) = pending.pop() {
            if std::mem::replace(&mut reachable[block], true) {
                continue;
            }
            let Some(last) = func.layout.last_inst(block) else { continue };
            match func.dfg.insts[last] {
                InstructionData::TryCall { exception, .. }
                | InstructionData::TryCallIndirect { exception, .. } => {
                    let returns = func.dfg.exception_tables[exception].normal_return();
                    pending.push(returns.block(&func.dfg.value_lists));
                }
                data => pending.extend(
                    data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .iter()
                        .map(|call| call.block(&func.dfg.value_lists)),
                ),
            }
        }
        reachable
    }

    fn i(&mut self, instruction: I<'_>) {
        instruction.encode(&mut self.code);
    }

    /// Reads the stack pointer, or writes it. Which global that is, a linker says.
    fn stack_pointer(&mut self, write: bool) {
        self.code.push(if write { 0x24 } else { 0x23 });
        self.stack_pointer_fixups.push(self.code.len() as u32);
        self.index_to_fill();
    }

    /// Leaves room for an index: a LEB128 of full length, so that any index fits.
    fn index_to_fill(&mut self) {
        self.code.extend([0x80; INDEX_LEN - 1]);
        self.code.push(0);
    }

    fn temp(&mut self, ty: ValType) -> u32 {
        self.locals.push(ty);
        self.param_count + self.locals.len() as u32 - 1
    }

    fn ty(&self, value: Value) -> Type {
        self.func.dfg.value_type(value)
    }

    fn class(&self, value: Value) -> Result<Class, String> {
        Class::of(self.ty(value))
    }

    /// The first local of a value.
    fn local(&mut self, value: Value) -> Result<u32, String> {
        let value = self.func.dfg.resolve_aliases(value);
        if self.values[value] != 0 {
            return Ok(self.values[value] - 1);
        }
        let class = self.class(value)?;
        let local = self.temp(class.wasm().into());
        if class == Class::Pair {
            self.temp(ValType::I64);
        }
        self.values[value] = local + 1;
        Ok(local)
    }

    /// Pushes a value: both halves of a wide integer, the low one first.
    fn get(&mut self, value: Value) -> Result<(), String> {
        let local = self.local(value)?;
        self.i(I::LocalGet(local));
        if self.class(value)? == Class::Pair {
            self.i(I::LocalGet(local + 1));
        }
        Ok(())
    }

    /// Pops a value pushed the way [`Self::get`] pushes.
    fn set(&mut self, value: Value) -> Result<(), String> {
        let local = self.local(value)?;
        if self.class(value)? == Class::Pair {
            self.i(I::LocalSet(local + 1));
        }
        self.i(I::LocalSet(local));
        Ok(())
    }

    fn get_low(&mut self, value: Value) -> Result<(), String> {
        let local = self.local(value)?;
        self.i(I::LocalGet(local));
        Ok(())
    }

    fn get_high(&mut self, value: Value) -> Result<(), String> {
        let local = self.local(value)? + 1;
        self.i(I::LocalGet(local));
        Ok(())
    }

    /// Pushes a narrow value sign-extended to the `i32` it is held in.
    fn get_signed(&mut self, value: Value) -> Result<(), String> {
        self.get(value)?;
        match self.class(value)?.narrow() {
            Some(8) => self.i(I::I32Extend8S),
            Some(_) => self.i(I::I32Extend16S),
            None => {}
        }
        Ok(())
    }

    fn get_for(&mut self, value: Value, signed: bool) -> Result<(), String> {
        if signed { self.get_signed(value) } else { self.get(value) }
    }

    /// Clears the bits of the `i32` on the stack that a narrow integer has no use for.
    fn mask(&mut self, bits: u8) {
        self.i(I::I32Const(((1u32 << bits) - 1) as i32));
        self.i(I::I32And);
    }

    fn mask_for(&mut self, class: Class) {
        if let Some(bits) = class.narrow() {
            self.mask(bits);
        }
    }

    fn op(&mut self, op: Op, long: bool) {
        self.i(op.instruction(long));
    }

    fn constant(&mut self, value: i64, long: bool) {
        if long {
            self.i(I::I64Const(value));
        } else {
            self.i(I::I32Const(value as i32));
        }
    }

    /// Pushes whether a value is anything but zero.
    fn truthy(&mut self, value: Value) -> Result<(), String> {
        match self.class(value)? {
            Class::Word(_) => self.get(value)?,
            Class::Long => {
                self.get(value)?;
                self.i(I::I64Const(0));
                self.i(I::I64Ne);
            }
            Class::Pair => {
                self.get(value)?;
                self.i(I::I64Or);
                self.i(I::I64Const(0));
                self.i(I::I64Ne);
            }
            // alloc: the error's message
            Class::F32 | Class::F64 => return Err("wasm: float as a condition".to_owned()),
            // alloc: the error's message
            Class::V128 => return Err("wasm: vector as a condition".to_owned()),
        }
        Ok(())
    }

    /// Pushes an integer as a shift amount for an operand that is an `i64` or is not.
    fn amount(&mut self, value: Value, long: bool) -> Result<(), String> {
        match (self.class(value)?, long) {
            (Class::Word(_), false) | (Class::Long, true) => self.get(value)?,
            (Class::Word(_), true) => {
                self.get(value)?;
                self.i(I::I64ExtendI32U);
            }
            (Class::Long, false) => {
                self.get(value)?;
                self.i(I::I32WrapI64);
            }
            (Class::Pair, _) => {
                self.get_low(value)?;
                if !long {
                    self.i(I::I32WrapI64);
                }
            }
            // alloc: the error's message
            _ => return Err("wasm: float as a shift amount".to_owned()),
        }
        Ok(())
    }

    /// Pushes an address that a linker fills in.
    fn address(&mut self, name: &ExternalName, addend: i64) -> Result<(), String> {
        let ExternalName::User(name) = name else {
            // alloc: the error's message
            return Err(format!("wasm: address of {name:?}"));
        };
        let name = &self.func.params.user_named_funcs()[*name];
        // `i32.const`, then the operand.
        self.code.push(0x41);
        self.relocs.push(ModuleReloc {
            offset: self.code.len() as u32,
            kind: cranelift_codegen::binemit::Reloc::Abs4,
            name: ModuleRelocTarget::user(name.namespace, name.index),
            addend,
        });
        self.index_to_fill();
        Ok(())
    }

    /// Calls the function `name` with the arguments on the stack.
    fn call_named(&mut self, name: &ExternalName, signature: &Signature) -> Result<(), String> {
        if self.calls == Calls::Table {
            self.address(name, 0)?;
            return self.call_slot(signature);
        }
        let ExternalName::User(name) = name else {
            // alloc: the error's message
            return Err(format!("wasm: call of {name:?}"));
        };
        let name = &self.func.params.user_named_funcs()[*name];
        // `call`, then the index of the function.
        self.code.push(0x10);
        self.relocs.push(ModuleReloc {
            offset: self.code.len() as u32,
            kind: CALL,
            name: ModuleRelocTarget::user(name.namespace, name.index),
            addend: 0,
        });
        self.index_to_fill();
        Ok(())
    }

    /// Calls through the table the function whose slot is on the stack.
    fn call_slot(&mut self, signature: &Signature) -> Result<(), String> {
        let signature = WasmSignature::of(signature)?;
        let index = match self.types.iter().position(|it| *it == signature) {
            Some(index) => index,
            None => {
                self.types.push(signature);
                self.types.len() - 1
            }
        };
        // `call_indirect`, the type, the table.
        self.code.push(0x11);
        self.type_fixups.push((self.code.len() as u32, index as u32));
        self.index_to_fill();
        self.code.push(0);
        Ok(())
    }

    fn slot_address(&mut self, slot: ir::StackSlot, offset: i64) -> Result<(), String> {
        let Some((frame, _)) = self.frame else {
            // alloc: the error's message
            return Err("wasm: stack slot without a frame".to_owned());
        };
        let offset = self.slot_offsets[slot.index()] as i64 + offset;
        self.i(I::LocalGet(frame));
        if offset != 0 {
            self.i(I::I32Const(offset as i32));
            self.i(I::I32Add);
        }
        Ok(())
    }

    /// Pushes the address `base + offset` and gives what is left of the offset to put in the
    /// instruction.
    fn pointer(&mut self, base: Value, offset: i64) -> Result<u64, String> {
        self.get(base)?;
        if offset >= 0 {
            return Ok(offset as u64);
        }
        self.i(I::I32Const(offset as i32));
        self.i(I::I32Add);
        Ok(0)
    }

    /// Loads `bits` of an integer held as `class` from the address on the stack.
    fn load_int(&mut self, class: Class, bits: u8, signed: bool, offset: u64) {
        let at = mem(offset);
        self.i(match (class.is_long() || class == Class::Pair, bits, signed) {
            (false, 8, false) => I::I32Load8U(at),
            (false, 8, true) => I::I32Load8S(at),
            (false, 16, false) => I::I32Load16U(at),
            (false, 16, true) => I::I32Load16S(at),
            (false, ..) => I::I32Load(at),
            (true, 8, false) => I::I64Load8U(at),
            (true, 8, true) => I::I64Load8S(at),
            (true, 16, false) => I::I64Load16U(at),
            (true, 16, true) => I::I64Load16S(at),
            (true, 32, false) => I::I64Load32U(at),
            (true, 32, true) => I::I64Load32S(at),
            (true, ..) => I::I64Load(at),
        });
    }

    /// Stores the low `bits` of the integer on the stack, held as `class`, to the address
    /// under it.
    fn store_int(&mut self, class: Class, bits: u8, offset: u64) {
        let at = mem(offset);
        self.i(match (class.is_long() || class == Class::Pair, bits) {
            (false, 8) => I::I32Store8(at),
            (false, 16) => I::I32Store16(at),
            (false, _) => I::I32Store(at),
            (true, 8) => I::I64Store8(at),
            (true, 16) => I::I64Store16(at),
            (true, 32) => I::I64Store32(at),
            (true, _) => I::I64Store(at),
        });
    }

    fn load(&mut self, result: Value, base: Value, offset: i64) -> Result<(), String> {
        let class = self.class(result)?;
        let offset = self.pointer(base, offset)?;
        match class {
            Class::Word(bits) => self.load_int(class, bits, false, offset),
            Class::Long => self.load_int(class, 64, false, offset),
            Class::Pair => {
                self.i(I::I64Load(mem(offset)));
                self.get(base)?;
                self.i(I::I64Load(mem(offset + 8)));
            }
            Class::F32 => self.i(I::F32Load(mem(offset))),
            Class::F64 => self.i(I::F64Load(mem(offset))),
            Class::V128 => self.i(I::V128Load(mem(offset))),
        }
        self.set(result)
    }

    fn store(&mut self, value: Value, base: Value, offset: i64) -> Result<(), String> {
        let class = self.class(value)?;
        let offset = self.pointer(base, offset)?;
        match class {
            Class::Word(bits) => {
                self.get(value)?;
                self.store_int(class, bits, offset);
            }
            Class::Long => {
                self.get(value)?;
                self.i(I::I64Store(mem(offset)));
            }
            Class::Pair => {
                self.get_low(value)?;
                self.i(I::I64Store(mem(offset)));
                self.get(base)?;
                self.get_high(value)?;
                self.i(I::I64Store(mem(offset + 8)));
            }
            Class::F32 => {
                self.get(value)?;
                self.i(I::F32Store(mem(offset)));
            }
            Class::F64 => {
                self.get(value)?;
                self.i(I::F64Store(mem(offset)));
            }
            Class::V128 => {
                self.get(value)?;
                self.i(I::V128Store(mem(offset)));
            }
        }
        Ok(())
    }

    /// Leaves the block for another: `depth` is how many wasm blocks opened inside the
    /// block's own code the jump is in.
    fn jump(
        &mut self,
        from: Block,
        call: BlockCall,
        depth: u32,
        returned: &[u32],
    ) -> Result<(), String> {
        let func = self.func;
        let target = call.block(&func.dfg.value_lists);
        let params = func.dfg.block_params(target);
        let mut passed = 0;
        for (arg, &param) in call.args(&func.dfg.value_lists).zip(params) {
            match arg {
                BlockArg::Value(value) => self.get(value)?,
                BlockArg::TryCallRet(index) => {
                    let local = *returned.get(index as usize).ok_or("wasm: no such result")?;
                    self.i(I::LocalGet(local));
                    if self.class(param)? == Class::Pair {
                        self.i(I::LocalGet(local + 1));
                    }
                }
                // alloc: the error's message
                BlockArg::TryCallExn(_) => return Err("wasm: exception payload".to_owned()),
            }
            passed += 1;
        }
        // All of them are on the stack before any is written: a block may pass its own
        // parameters on in another order.
        for &param in params[..passed].iter().rev() {
            self.set(param)?;
        }
        let (from, target) = (self.order[from] - 1, self.order[target] - 1);
        if target > from {
            self.i(I::Br(target - from - 1 + depth));
            return Ok(());
        }
        let Some(next) = self.next_block else {
            // alloc: the error's message
            return Err("wasm: jump backwards without a loop".to_owned());
        };
        self.i(I::I32Const(target as i32));
        self.i(I::LocalSet(next));
        self.i(I::Br(self.block_count - 1 - from + depth));
        Ok(())
    }

    fn return_(&mut self, values: &[Value]) -> Result<(), String> {
        let returned = std::mem::replace(&mut self.returned, Returned::Nothing);
        let stored = self.return_values(&returned, values);
        self.returned = returned;
        stored?;
        if let Some((frame, size)) = self.frame {
            self.i(I::LocalGet(frame));
            self.i(I::I32Const(size as i32));
            self.i(I::I32Add);
            self.stack_pointer(true);
        }
        self.i(I::Return);
        Ok(())
    }

    /// Hands back `values` the way `returned` says, short of leaving the frame.
    fn return_values(&mut self, returned: &Returned, values: &[Value]) -> Result<(), String> {
        match returned {
            Returned::Nothing => {}
            Returned::Value(_) => {
                let [value] = values else {
                    // alloc: the error's message
                    return Err("wasm: return and signature disagree".to_owned());
                };
                let signed =
                    self.func.signature.returns[0].extension == ir::ArgumentExtension::Sext;
                self.get_for(*value, signed)?;
            }
            Returned::Through(places) => {
                for (&value, &(at, class)) in values.iter().zip(places) {
                    self.i(I::LocalGet(0));
                    self.get(value)?;
                    self.store_class(class, at);
                }
            }
        }
        Ok(())
    }

    /// Stores the value on the stack, held as `class`, at `offset` from the address below it.
    fn store_class(&mut self, class: Class, offset: u64) {
        match class {
            Class::Word(bits) => self.store_int(class, bits, offset),
            Class::Long => self.i(I::I64Store(mem(offset))),
            Class::F32 => self.i(I::F32Store(mem(offset))),
            Class::F64 => self.i(I::F64Store(mem(offset))),
            Class::V128 => self.i(I::V128Store(mem(offset))),
            Class::Pair => {
                let high = self.temp(ValType::I64);
                let low = self.temp(ValType::I64);
                let address = self.temp(ValType::I32);
                self.i(I::LocalSet(high));
                self.i(I::LocalSet(low));
                self.i(I::LocalTee(address));
                self.i(I::LocalGet(low));
                self.i(I::I64Store(mem(offset)));
                self.i(I::LocalGet(address));
                self.i(I::LocalGet(high));
                self.i(I::I64Store(mem(offset + 8)));
            }
        }
    }

    /// Pushes the value held as `class` at `offset` from the address on the stack.
    fn load_class(&mut self, class: Class, offset: u64) {
        match class {
            Class::Word(bits) => self.load_int(class, bits, false, offset),
            Class::Long => self.i(I::I64Load(mem(offset))),
            Class::F32 => self.i(I::F32Load(mem(offset))),
            Class::F64 => self.i(I::F64Load(mem(offset))),
            Class::V128 => self.i(I::V128Load(mem(offset))),
            Class::Pair => {
                let address = self.temp(ValType::I32);
                self.i(I::LocalTee(address));
                self.i(I::I64Load(mem(offset)));
                self.i(I::LocalGet(address));
                self.i(I::I64Load(mem(offset + 8)));
            }
        }
    }

    /// Pushes the arguments of a call: the pointer the function returns through when it does
    /// that, then each one extended the way the signature asks.
    fn arguments(&mut self, signature: &Signature, args: &[Value]) -> Result<(), String> {
        if let Returned::Through(_) = Returned::of(signature)? {
            let Some(at) = self.returned_at else {
                // alloc: the error's message
                return Err("wasm: no room for what a call returns".to_owned());
            };
            let (frame, _) = self.frame.ok_or("wasm: no frame for what a call returns")?;
            self.i(I::LocalGet(frame));
            self.i(I::I32Const(at as i32));
            self.i(I::I32Add);
        }
        for (&arg, param) in args.iter().zip(&signature.params) {
            self.get_for(arg, param.extension == ir::ArgumentExtension::Sext)?;
        }
        Ok(())
    }

    /// Makes the call the arguments are on the stack for and puts what it returns in fresh
    /// locals, the first of each value given.
    fn finish_call(
        &mut self,
        callee: Option<&ExternalName>,
        signature: &Signature,
    ) -> Result<Returns, String> {
        match callee {
            Some(name) => self.call_named(name, signature)?,
            None => self.call_slot(signature)?,
        }
        let mut local_of = |this: &mut Self, class: Class| {
            let local = this.temp(class.wasm().into());
            if class == Class::Pair {
                this.temp(ValType::I64);
            }
            local
        };
        match Returned::of(signature)? {
            Returned::Nothing => Ok(Returns::new()),
            Returned::Value(class) => {
                let local = local_of(self, class);
                self.mask_for(class);
                self.i(I::LocalSet(local));
                let mut locals = Returns::new();
                locals.push(local);
                Ok(locals)
            }
            Returned::Through(places) => {
                let (frame, _) = self.frame.ok_or("wasm: no frame for what a call returns")?;
                let base = self.returned_at.ok_or("wasm: no room for what a call returns")?;
                let mut locals = Returns::new();
                for (at, class) in places {
                    let local = local_of(self, class);
                    self.i(I::LocalGet(frame));
                    self.load_class(class, base + at);
                    if class == Class::Pair {
                        self.i(I::LocalSet(local + 1));
                    }
                    self.mask_for(class);
                    self.i(I::LocalSet(local));
                    locals.push(local);
                }
                Ok(locals)
            }
        }
    }

    /// Calls a function. `callee` is `None` when the slot to call is the first of `args`.
    fn call(
        &mut self,
        callee: Option<&ExternalName>,
        signature: &Signature,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        if let Some(ExternalName::LibCall(libcall)) = callee {
            return self.libcall(*libcall, args, results);
        }
        if let Some(ExternalName::User(name)) = callee {
            let name = &self.func.params.user_named_funcs()[*name];
            if name.namespace == INSTRUCTION {
                return self.instruction(name.index, args, results);
            }
        }
        let returned = self.call_returning(callee, signature, args)?;
        for (&result, local) in results.iter().zip(returned) {
            self.i(I::LocalGet(local));
            if self.class(result)? == Class::Pair {
                self.i(I::LocalGet(local + 1));
            }
            self.set(result)?;
        }
        Ok(())
    }

    fn call_returning(
        &mut self,
        callee: Option<&ExternalName>,
        signature: &Signature,
        args: &[Value],
    ) -> Result<Returns, String> {
        match callee {
            Some(_) => self.arguments(signature, args)?,
            None => {
                let [slot, args @ ..] = args else {
                    // alloc: the error's message
                    return Err("wasm: indirect call of nothing".to_owned());
                };
                self.arguments(signature, args)?;
                self.get(*slot)?;
            }
        }
        self.finish_call(callee, signature)
    }

    /// The memory routines Cranelift's frontend calls by name are instructions here.
    fn libcall(
        &mut self,
        libcall: LibCall,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        match (libcall, args) {
            (LibCall::Memcpy | LibCall::Memmove, &[to, from, len]) => {
                self.get(to)?;
                self.get(from)?;
                self.get(len)?;
                self.i(I::MemoryCopy { src_mem: MEMORY, dst_mem: MEMORY });
            }
            (LibCall::Memset, &[to, byte, len]) => {
                self.get(to)?;
                self.get(byte)?;
                self.get(len)?;
                self.i(I::MemoryFill(MEMORY));
            }
            // alloc: the error's message
            _ => return Err(format!("wasm: libcall {libcall}")),
        }
        if let (&[to, ..], &[result]) = (args, results) {
            self.get(to)?;
            // Cranelift's frontend types what the routine returns as an address of the
            // machine it has a backend for.
            if self.func.dfg.value_type(result) == types::I64 {
                self.i(I::I64ExtendI32U);
            }
            self.set(result)?;
        }
        Ok(())
    }

    /// Calls as [`Self::call`] does and goes on to where the call returns to. Nothing here
    /// unwinds, so the handlers are never reached.
    fn try_call(
        &mut self,
        block: Block,
        callee: Option<&ExternalName>,
        table: ir::ExceptionTable,
        args: &[Value],
    ) -> Result<(), String> {
        let func = self.func;
        let table = &func.dfg.exception_tables[table];
        let signature = &func.dfg.signatures[table.signature()];
        // A memory routine of Cranelift's frontend is an instruction ([`Self::libcall`]): what
        // it returns is the address it was given.
        if let Some(ExternalName::LibCall(libcall)) = callee {
            self.libcall(*libcall, args, &[])?;
            let mut returned = Returns::new();
            for ret in &signature.returns {
                let wide = ret.value_type == types::I64;
                let local = self.temp(if wide { ValType::I64 } else { ValType::I32 });
                self.get(*args.first().ok_or("wasm: a memory routine of no address")?)?;
                if wide {
                    self.i(I::I64ExtendI32U);
                }
                self.i(I::LocalSet(local));
                returned.push(local);
            }
            return self.jump(block, *table.normal_return(), 0, &returned);
        }
        let returned = self.call_returning(callee, signature, args)?;
        self.jump(block, *table.normal_return(), 0, &returned)
    }

    fn inst(&mut self, block: Block, inst: ir::Inst) -> Result<(), String> {
        let func = self.func;
        let dfg = &func.dfg;
        let results = dfg.inst_results(inst);
        let data = dfg.insts[inst];
        let opcode = data.opcode();
        match data {
            InstructionData::Jump { destination, .. } => self.jump(block, destination, 0, &[]),
            InstructionData::Brif { arg, blocks: [then, otherwise], .. } => {
                self.truthy(arg)?;
                self.i(I::If(BlockType::Empty));
                self.jump(block, then, 1, &[])?;
                self.i(I::End);
                self.jump(block, otherwise, 0, &[])
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let table = &dfg.jump_tables[table];
                let cases = table.as_slice();
                // One wasm block per case to land in, where the jump proper is: a case
                // passes arguments of its own.
                for _ in 0..=cases.len() {
                    self.i(I::Block(BlockType::Empty));
                }
                match self.class(arg)? {
                    Class::Word(_) => self.get(arg)?,
                    Class::Long => {
                        // An index that does not fit goes to the default like any other
                        // that is out of range.
                        let count = cases.len() as i32;
                        self.i(I::I32Const(count));
                        self.get(arg)?;
                        self.i(I::I32WrapI64);
                        self.get(arg)?;
                        self.i(I::I64Const(i64::from(count)));
                        self.i(I::I64GeU);
                        self.i(I::Select);
                    }
                    // alloc: the error's message
                    _ => return Err("wasm: br_table on a wide value".to_owned()),
                }
                let count = cases.len() as u32;
                InstructionSink::new(&mut self.code).br_table(0..count, count);
                for (at, &case) in cases.iter().enumerate() {
                    self.i(I::End);
                    self.jump(block, case, count - at as u32, &[])?;
                }
                self.i(I::End);
                self.jump(block, table.default_block(), 0, &[])
            }
            InstructionData::MultiAry { opcode: Opcode::Return, args } => {
                let args = args.as_slice(&dfg.value_lists);
                self.return_(args)
            }
            InstructionData::Trap { .. } => {
                self.i(I::Unreachable);
                Ok(())
            }
            InstructionData::CondTrap { arg, .. } => {
                self.truthy(arg)?;
                if opcode == Opcode::Trapz {
                    self.i(I::I32Eqz);
                }
                self.i(I::If(BlockType::Empty));
                self.i(I::Unreachable);
                self.i(I::End);
                Ok(())
            }
            InstructionData::Call { args, func_ref, .. } => {
                let callee = &dfg.ext_funcs[func_ref];
                let signature = &dfg.signatures[callee.signature];
                let args = args.as_slice(&dfg.value_lists);
                self.call(Some(&callee.name), signature, args, results)
            }
            InstructionData::CallIndirect { args, sig_ref, .. } => {
                let args = args.as_slice(&dfg.value_lists);
                self.call(None, &dfg.signatures[sig_ref], args, results)
            }
            InstructionData::TryCall { args, func_ref, exception, .. } => {
                let callee = &dfg.ext_funcs[func_ref];
                let args = args.as_slice(&dfg.value_lists);
                self.try_call(block, Some(&callee.name), exception, args)
            }
            InstructionData::TryCallIndirect { args, exception, .. } => {
                let args = args.as_slice(&dfg.value_lists);
                self.try_call(block, None, exception, args)
            }
            InstructionData::FuncAddr { func_ref, .. } => {
                self.address(&dfg.ext_funcs[func_ref].name, 0)?;
                self.set(results[0])
            }
            InstructionData::UnaryGlobalValue { global_value, .. } => {
                let GlobalValueData::Symbol { name, offset, .. } =
                    &func.global_values[global_value]
                else {
                    // alloc: the error's message
                    return Err("wasm: global value that is not a symbol".to_owned());
                };
                self.address(name, offset.bits())?;
                self.set(results[0])
            }
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                self.slot_address(stack_slot, offset.into())?;
                self.set(results[0])
            }
            InstructionData::UnaryImm { imm, .. } => {
                let class = self.class(results[0])?;
                match class {
                    Class::Word(bits) => {
                        let mask = if bits == 32 { u32::MAX } else { (1 << bits) - 1 };
                        self.i(I::I32Const((imm.bits() as u32 & mask) as i32));
                    }
                    Class::Long => self.i(I::I64Const(imm.bits())),
                    // The immediate is the low half, zero-extended as Cranelift's backends do.
                    Class::Pair => {
                        self.i(I::I64Const(imm.bits()));
                        self.i(I::I64Const(0));
                    }
                    // alloc: the error's message
                    _ => return Err(format!("wasm: iconst of {class:?}")),
                }
                self.set(results[0])
            }
            InstructionData::UnaryIeee32 { imm, .. } => {
                self.i(I::F32Const(f32::from_bits(imm.bits()).into()));
                self.set(results[0])
            }
            InstructionData::UnaryIeee64 { imm, .. } => {
                self.i(I::F64Const(f64::from_bits(imm.bits()).into()));
                self.set(results[0])
            }
            InstructionData::Load { arg, offset, .. } => match opcode {
                Opcode::Load => self.load(results[0], arg, offset.into()),
                Opcode::Uload8x8
                | Opcode::Sload8x8
                | Opcode::Uload16x4
                | Opcode::Sload16x4
                | Opcode::Uload32x2
                | Opcode::Sload32x2 => self.load_extend(opcode, results[0], arg, offset.into()),
                _ => {
                    let (bits, signed) = match opcode {
                        Opcode::Uload8 => (8, false),
                        Opcode::Sload8 => (8, true),
                        Opcode::Uload16 => (16, false),
                        Opcode::Sload16 => (16, true),
                        Opcode::Uload32 => (32, false),
                        Opcode::Sload32 => (32, true),
                        // alloc: the error's message
                        _ => return Err(format!("wasm: {opcode}")),
                    };
                    let class = self.class(results[0])?;
                    let offset = self.pointer(arg, offset.into())?;
                    self.load_int(class, bits, signed, offset);
                    self.mask_for(class);
                    self.set(results[0])
                }
            },
            InstructionData::Store { args: [value, base], offset, .. } => match opcode {
                Opcode::Store => self.store(value, base, offset.into()),
                _ => {
                    let bits = match opcode {
                        Opcode::Istore8 => 8,
                        Opcode::Istore16 => 16,
                        Opcode::Istore32 => 32,
                        // alloc: the error's message
                        _ => return Err(format!("wasm: {opcode}")),
                    };
                    let class = self.class(value)?;
                    let offset = self.pointer(base, offset.into())?;
                    self.get_low(value)?;
                    self.store_int(class, bits, offset);
                    Ok(())
                }
            },
            InstructionData::LoadNoOffset { arg, .. } => match opcode {
                Opcode::Bitcast => self.bitcast(results[0], arg),
                // One thread runs the code: an atomic load is a load.
                Opcode::AtomicLoad => self.load(results[0], arg, 0),
                // alloc: the error's message
                _ => Err(format!("wasm: {opcode}")),
            },
            InstructionData::StoreNoOffset { args: [value, base], .. } => match opcode {
                Opcode::AtomicStore => self.store(value, base, 0),
                // alloc: the error's message
                _ => Err(format!("wasm: {opcode}")),
            },
            InstructionData::AtomicRmw { args: [base, value], op, .. } => {
                self.atomic_rmw(results[0], base, value, op)
            }
            InstructionData::AtomicCas { args: [base, expected, replacement], .. } => {
                let old = results[0];
                self.load(old, base, 0)?;
                match self.class(old)? {
                    Class::Word(_) => {
                        self.get(old)?;
                        self.get(expected)?;
                        self.i(I::I32Eq);
                    }
                    Class::Long => {
                        self.get(old)?;
                        self.get(expected)?;
                        self.i(I::I64Eq);
                    }
                    // alloc: the error's message
                    _ => return Err("wasm: atomic_cas of a wide type".to_owned()),
                }
                self.i(I::If(BlockType::Empty));
                self.store(replacement, base, 0)?;
                self.i(I::End);
                Ok(())
            }
            InstructionData::IntCompare { args: [a, b], cond, .. } => {
                if self.class(a)? == Class::V128 {
                    return self.vector_compare(results[0], cond, a, b);
                }
                self.compare(Op::compare(cond), a, b)?;
                self.set(results[0])
            }
            InstructionData::FloatCompare { args: [a, b], cond, .. } => {
                if self.class(a)? == Class::V128 {
                    return self.vector_float_compare(results[0], cond, a, b);
                }
                self.float_compare(cond, a, b)?;
                self.set(results[0])
            }
            InstructionData::UnaryConst { constant_handle, .. } => {
                let bytes = func.dfg.constants.get(constant_handle).as_slice();
                let bytes: [u8; 16] =
                    bytes.try_into().map_err(|_| "wasm: vconst of other than 16 bytes")?;
                self.i(I::V128Const(i128::from_le_bytes(bytes)));
                self.set(results[0])
            }
            InstructionData::Shuffle { args: [a, b], imm, .. } => {
                let mask = func.dfg.immediates.get(imm).ok_or("wasm: shuffle without a mask")?;
                let lanes: [u8; 16] = mask
                    .as_slice()
                    .try_into()
                    .map_err(|_| "wasm: shuffle mask of other than 16 bytes")?;
                if lanes.iter().any(|&lane| lane >= 32) {
                    // alloc: the error's message
                    return Err("wasm: shuffle of a lane past both vectors".to_owned());
                }
                self.get(a)?;
                self.get(b)?;
                self.i(I::I8x16Shuffle(lanes));
                self.set(results[0])
            }
            InstructionData::BinaryImm8 { opcode: Opcode::Extractlane, arg, imm } => {
                self.extract_lane(results[0], arg, imm)
            }
            InstructionData::TernaryImm8 {
                opcode: Opcode::Insertlane,
                args: [vector, lane],
                imm,
            } => self.insert_lane(results[0], vector, lane, imm),
            InstructionData::Ternary { args: [a, b, c], .. } => match opcode {
                Opcode::Select | Opcode::SelectSpectreGuard => self.select(results[0], a, b, c),
                Opcode::Bitselect if self.class(b)? == Class::V128 => {
                    self.get(b)?;
                    self.get(c)?;
                    self.get(a)?;
                    self.i(I::V128Bitselect);
                    self.set(results[0])
                }
                Opcode::Bitselect if self.class(b)? == Class::Pair => {
                    let local = self.local(results[0])?;
                    for half in [0, 1] {
                        let (b, c, a) = (self.local(b)?, self.local(c)?, self.local(a)?);
                        self.i(I::LocalGet(b + half));
                        self.i(I::LocalGet(a + half));
                        self.i(I::I64And);
                        self.i(I::LocalGet(c + half));
                        self.i(I::LocalGet(a + half));
                        self.i(I::I64Const(-1));
                        self.i(I::I64Xor);
                        self.i(I::I64And);
                        self.i(I::I64Or);
                        self.i(I::LocalSet(local + half));
                    }
                    Ok(())
                }
                Opcode::Bitselect => {
                    let long = match self.class(b)? {
                        Class::Word(_) => false,
                        Class::Long => true,
                        // alloc: the error's message
                        _ => return Err("wasm: bitselect of this type".to_owned()),
                    };
                    self.get(b)?;
                    self.get(a)?;
                    self.op(Op::And, long);
                    self.get(c)?;
                    self.get(a)?;
                    self.constant(-1, long);
                    self.op(Op::Xor, long);
                    self.op(Op::And, long);
                    self.op(Op::Or, long);
                    self.mask_for(self.class(b)?);
                    self.set(results[0])
                }
                // alloc: the error's message
                _ => Err(format!("wasm: {opcode}")),
            },
            InstructionData::Unary { arg, .. } => self.unary(opcode, results, arg),
            InstructionData::Binary { args: [a, b], .. } => self.binary(opcode, results, a, b),
            InstructionData::NullAry { .. } => match opcode {
                Opcode::Nop | Opcode::Fence => Ok(()),
                // alloc: the error's message
                _ => Err(format!("wasm: {opcode}")),
            },
            // alloc: the error's message
            _ => Err(format!("wasm: {opcode}")),
        }
    }

    fn select(
        &mut self,
        result: Value,
        cond: Value,
        then: Value,
        otherwise: Value,
    ) -> Result<(), String> {
        if self.class(then)? == Class::V128 {
            self.get(then)?;
            self.get(otherwise)?;
            self.truthy(cond)?;
            self.i(I::TypedSelect(ValType::V128));
            return self.set(result);
        }
        if self.class(then)? == Class::Pair {
            let local = self.local(result)?;
            self.get_low(then)?;
            self.get_low(otherwise)?;
            self.truthy(cond)?;
            self.i(I::Select);
            self.get_high(then)?;
            self.get_high(otherwise)?;
            self.truthy(cond)?;
            self.i(I::Select);
            self.i(I::LocalSet(local + 1));
            self.i(I::LocalSet(local));
            return Ok(());
        }
        self.get(then)?;
        self.get(otherwise)?;
        self.truthy(cond)?;
        self.i(I::Select);
        self.set(result)
    }

    /// Pushes the `i32` that is one when the comparison holds.
    fn compare(&mut self, op: Op, a: Value, b: Value) -> Result<(), String> {
        match self.class(a)? {
            class @ (Class::Word(_) | Class::Long) => {
                self.get_for(a, op.is_signed())?;
                self.get_for(b, op.is_signed())?;
                self.op(op, class.is_long());
            }
            Class::Pair => match op {
                Op::Eq | Op::Ne => {
                    self.get_low(a)?;
                    self.get_low(b)?;
                    self.i(I::I64Xor);
                    self.get_high(a)?;
                    self.get_high(b)?;
                    self.i(I::I64Xor);
                    self.i(I::I64Or);
                    self.i(I::I64Eqz);
                    if matches!(op, Op::Ne) {
                        self.i(I::I32Eqz);
                    }
                }
                _ => {
                    // The high halves decide unless they are equal, and then the low ones
                    // do, unsigned.
                    self.get_low(a)?;
                    self.get_low(b)?;
                    self.op(
                        if op.or_equal() {
                            op.strict_unsigned_or_equal()
                        } else {
                            op.strict_unsigned()
                        },
                        true,
                    );
                    self.get_high(a)?;
                    self.get_high(b)?;
                    self.op(op.strict(), true);
                    self.get_high(a)?;
                    self.get_high(b)?;
                    self.i(I::I64Eq);
                    self.i(I::Select);
                }
            },
            // alloc: the error's message
            _ => return Err("wasm: icmp of floats".to_owned()),
        }
        Ok(())
    }

    fn float_compare(&mut self, cond: FloatCC, a: Value, b: Value) -> Result<(), String> {
        let single = self.class(a)? == Class::F32;
        let pick = |single: bool, f32: I<'static>, f64: I<'static>| if single { f32 } else { f64 };
        let eq = pick(single, I::F32Eq, I::F64Eq);
        let ne = pick(single, I::F32Ne, I::F64Ne);
        let lt = pick(single, I::F32Lt, I::F64Lt);
        let le = pick(single, I::F32Le, I::F64Le);
        let gt = pick(single, I::F32Gt, I::F64Gt);
        let ge = pick(single, I::F32Ge, I::F64Ge);
        // Each is one comparison, or the opposite of one: an unordered pair fails every
        // comparison but `ne`.
        let (first, negate) = match cond {
            FloatCC::Equal => (eq, false),
            FloatCC::NotEqual => (ne, false),
            FloatCC::LessThan => (lt, false),
            FloatCC::LessThanOrEqual => (le, false),
            FloatCC::GreaterThan => (gt, false),
            FloatCC::GreaterThanOrEqual => (ge, false),
            FloatCC::UnorderedOrLessThan => (ge, true),
            FloatCC::UnorderedOrLessThanOrEqual => (gt, true),
            FloatCC::UnorderedOrGreaterThan => (le, true),
            FloatCC::UnorderedOrGreaterThanOrEqual => (lt, true),
            FloatCC::Ordered | FloatCC::Unordered => {
                self.get(a)?;
                self.get(a)?;
                self.i(pick(single, I::F32Eq, I::F64Eq));
                self.get(b)?;
                self.get(b)?;
                self.i(eq);
                self.i(I::I32And);
                if cond == FloatCC::Unordered {
                    self.i(I::I32Eqz);
                }
                return Ok(());
            }
            FloatCC::OrderedNotEqual | FloatCC::UnorderedOrEqual => {
                self.get(a)?;
                self.get(b)?;
                self.i(lt);
                self.get(a)?;
                self.get(b)?;
                self.i(gt);
                self.i(I::I32Or);
                if cond == FloatCC::UnorderedOrEqual {
                    self.i(I::I32Eqz);
                }
                return Ok(());
            }
        };
        self.get(a)?;
        self.get(b)?;
        self.i(first);
        if negate {
            self.i(I::I32Eqz);
        }
        Ok(())
    }

    fn bitcast(&mut self, result: Value, arg: Value) -> Result<(), String> {
        match (self.class(arg)?, self.class(result)?) {
            (Class::Pair, Class::V128) => {
                self.get_low(arg)?;
                self.i(I::I64x2Splat);
                self.get_high(arg)?;
                self.i(I::I64x2ReplaceLane(1));
                return self.set(result);
            }
            (Class::V128, Class::Pair) => {
                let local = self.local(result)?;
                for half in [0, 1] {
                    self.get(arg)?;
                    self.i(I::I64x2ExtractLane(half as u8));
                    self.i(I::LocalSet(local + half));
                }
                return Ok(());
            }
            _ => {}
        }
        self.get(arg)?;
        match (self.class(arg)?, self.class(result)?) {
            (Class::Word(32), Class::F32) => self.i(I::F32ReinterpretI32),
            (Class::F32, Class::Word(32)) => self.i(I::I32ReinterpretF32),
            (Class::Long, Class::F64) => self.i(I::F64ReinterpretI64),
            (Class::F64, Class::Long) => self.i(I::I64ReinterpretF64),
            (from, to) if from == to => {}
            // alloc: the error's message
            (from, to) => return Err(format!("wasm: bitcast from {from:?} to {to:?}")),
        }
        self.set(result)
    }

    fn atomic_rmw(
        &mut self,
        old: Value,
        base: Value,
        value: Value,
        op: AtomicRmwOp,
    ) -> Result<(), String> {
        let class = self.class(old)?;
        let long = match class {
            Class::Word(_) => false,
            Class::Long => true,
            // alloc: the error's message
            _ => return Err("wasm: atomic_rmw of a wide type".to_owned()),
        };
        self.load(old, base, 0)?;
        self.get(base)?;
        match op {
            AtomicRmwOp::Xchg => self.get(value)?,
            AtomicRmwOp::Add
            | AtomicRmwOp::Sub
            | AtomicRmwOp::And
            | AtomicRmwOp::Or
            | AtomicRmwOp::Xor => {
                self.get(old)?;
                self.get(value)?;
                self.op(
                    match op {
                        AtomicRmwOp::Add => Op::Add,
                        AtomicRmwOp::Sub => Op::Sub,
                        AtomicRmwOp::And => Op::And,
                        AtomicRmwOp::Or => Op::Or,
                        _ => Op::Xor,
                    },
                    long,
                );
            }
            AtomicRmwOp::Nand => {
                self.get(old)?;
                self.get(value)?;
                self.op(Op::And, long);
                self.constant(-1, long);
                self.op(Op::Xor, long);
            }
            AtomicRmwOp::Umin | AtomicRmwOp::Umax | AtomicRmwOp::Smin | AtomicRmwOp::Smax => {
                let pick = match op {
                    AtomicRmwOp::Umin => Op::LtU,
                    AtomicRmwOp::Umax => Op::GtU,
                    AtomicRmwOp::Smin => Op::LtS,
                    _ => Op::GtS,
                };
                self.get(old)?;
                self.get(value)?;
                self.compare(pick, old, value)?;
                self.i(I::Select);
            }
        }
        // The store keeps the low bits alone, so nothing has to be cleared.
        match class {
            Class::Word(bits) => self.store_int(class, bits, 0),
            _ => self.i(I::I64Store(mem(0))),
        }
        Ok(())
    }

    fn unary(&mut self, opcode: Opcode, results: &[Value], arg: Value) -> Result<(), String> {
        let from = self.class(arg)?;
        if from == Class::V128 || results.iter().any(|&it| self.class(it) == Ok(Class::V128)) {
            return self.unary_vector(opcode, results[0], arg);
        }
        if opcode == Opcode::Isplit {
            self.get_low(arg)?;
            self.set(results[0])?;
            self.get_high(arg)?;
            return self.set(results[1]);
        }
        let result = results[0];
        let to = self.class(result)?;
        let single = from == Class::F32;
        let float = |single: bool, f32: I<'static>, f64: I<'static>| if single { f32 } else { f64 };
        match opcode {
            Opcode::Uextend | Opcode::Sextend => {
                let signed = opcode == Opcode::Sextend;
                match (from, to) {
                    (Class::Word(_), Class::Word(_)) => {
                        self.get_for(arg, signed)?;
                        self.mask_for(to);
                    }
                    (Class::Word(_), Class::Long | Class::Pair) => {
                        self.get_for(arg, signed)?;
                        self.i(if signed { I::I64ExtendI32S } else { I::I64ExtendI32U });
                    }
                    (Class::Long, Class::Pair) => self.get(arg)?,
                    // alloc: the error's message
                    _ => return Err(format!("wasm: extend from {from:?} to {to:?}")),
                }
                if to == Class::Pair {
                    // The low half is on the stack; the high one is its sign, or nothing.
                    let local = self.local(result)?;
                    self.i(I::LocalTee(local));
                    self.i(I::I64Const(if signed { 63 } else { 64 }));
                    self.i(if signed { I::I64ShrS } else { I::I64ShrU });
                    if !signed {
                        self.i(I::Drop);
                        self.i(I::I64Const(0));
                    }
                    self.i(I::LocalSet(local + 1));
                    return Ok(());
                }
            }
            Opcode::Ireduce => {
                self.get_low(arg)?;
                if matches!(from, Class::Long | Class::Pair) && matches!(to, Class::Word(_)) {
                    self.i(I::I32WrapI64);
                }
                self.mask_for(to);
            }
            Opcode::Ineg => match to {
                Class::Word(_) | Class::Long => {
                    self.constant(0, to.is_long());
                    self.get(arg)?;
                    self.op(Op::Sub, to.is_long());
                    self.mask_for(to);
                }
                Class::Pair => {
                    // Nothing minus each half, the high one borrowing when the low is not zero.
                    self.i(I::I64Const(0));
                    self.get_low(arg)?;
                    self.i(I::I64Sub);
                    self.i(I::I64Const(0));
                    self.get_high(arg)?;
                    self.i(I::I64Sub);
                    self.get_low(arg)?;
                    self.i(I::I64Const(0));
                    self.i(I::I64Ne);
                    self.i(I::I64ExtendI32U);
                    self.i(I::I64Sub);
                }
                // alloc: the error's message
                _ => return Err("wasm: ineg of a float".to_owned()),
            },
            Opcode::Bnot => match to {
                Class::Word(_) | Class::Long => {
                    self.get(arg)?;
                    self.constant(-1, to.is_long());
                    self.op(Op::Xor, to.is_long());
                    self.mask_for(to);
                }
                Class::Pair => {
                    self.get_low(arg)?;
                    self.i(I::I64Const(-1));
                    self.i(I::I64Xor);
                    self.get_high(arg)?;
                    self.i(I::I64Const(-1));
                    self.i(I::I64Xor);
                }
                // alloc: the error's message
                _ => return Err("wasm: bnot of a float".to_owned()),
            },
            Opcode::Popcnt => match to {
                Class::Word(_) | Class::Long => {
                    self.get(arg)?;
                    self.op(Op::Popcnt, to.is_long());
                }
                Class::Pair => {
                    self.get_low(arg)?;
                    self.i(I::I64Popcnt);
                    self.get_high(arg)?;
                    self.i(I::I64Popcnt);
                    self.i(I::I64Add);
                    self.i(I::I64Const(0));
                }
                // alloc: the error's message
                _ => return Err("wasm: popcnt of a float".to_owned()),
            },
            Opcode::Clz => match to {
                Class::Word(bits) => {
                    self.get(arg)?;
                    self.i(I::I32Clz);
                    if bits < 32 {
                        self.i(I::I32Const(32 - i32::from(bits)));
                        self.i(I::I32Sub);
                    }
                }
                Class::Long => {
                    self.get(arg)?;
                    self.i(I::I64Clz);
                }
                Class::Pair => {
                    self.get_high(arg)?;
                    self.i(I::I64Clz);
                    self.get_low(arg)?;
                    self.i(I::I64Clz);
                    self.i(I::I64Const(64));
                    self.i(I::I64Add);
                    self.get_high(arg)?;
                    self.i(I::I64Const(0));
                    self.i(I::I64Ne);
                    self.i(I::Select);
                    self.i(I::I64Const(0));
                }
                // alloc: the error's message
                _ => return Err("wasm: clz of a float".to_owned()),
            },
            Opcode::Ctz => match to {
                Class::Word(bits) => {
                    self.get(arg)?;
                    if bits < 32 {
                        // A bit past the end stops the count there.
                        self.i(I::I32Const(1 << bits));
                        self.i(I::I32Or);
                    }
                    self.i(I::I32Ctz);
                }
                Class::Long => {
                    self.get(arg)?;
                    self.i(I::I64Ctz);
                }
                Class::Pair => {
                    self.get_low(arg)?;
                    self.i(I::I64Ctz);
                    self.get_high(arg)?;
                    self.i(I::I64Ctz);
                    self.i(I::I64Const(64));
                    self.i(I::I64Add);
                    self.get_low(arg)?;
                    self.i(I::I64Const(0));
                    self.i(I::I64Ne);
                    self.i(I::Select);
                    self.i(I::I64Const(0));
                }
                // alloc: the error's message
                _ => return Err("wasm: ctz of a float".to_owned()),
            },
            Opcode::Bswap => match to {
                Class::Word(16) => {
                    self.get(arg)?;
                    self.swap_bytes(false, 16);
                }
                Class::Word(32) => {
                    self.get(arg)?;
                    self.swap_bytes(false, 32);
                }
                Class::Long => {
                    self.get(arg)?;
                    self.swap_bytes(true, 64);
                }
                Class::Pair => {
                    self.get_high(arg)?;
                    self.swap_bytes(true, 64);
                    self.get_low(arg)?;
                    self.swap_bytes(true, 64);
                }
                // alloc: the error's message
                _ => return Err("wasm: bswap of this type".to_owned()),
            },
            Opcode::Bitrev => match to {
                Class::Word(bits) => {
                    self.get(arg)?;
                    self.reverse_bits(false, bits);
                }
                Class::Long => {
                    self.get(arg)?;
                    self.reverse_bits(true, 64);
                }
                Class::Pair => {
                    self.get_high(arg)?;
                    self.reverse_bits(true, 64);
                    self.get_low(arg)?;
                    self.reverse_bits(true, 64);
                }
                // alloc: the error's message
                _ => return Err("wasm: bitrev of a float".to_owned()),
            },
            Opcode::Fneg => {
                self.get(arg)?;
                self.i(float(single, I::F32Neg, I::F64Neg));
            }
            Opcode::Fabs => {
                self.get(arg)?;
                self.i(float(single, I::F32Abs, I::F64Abs));
            }
            Opcode::Sqrt => {
                self.get(arg)?;
                self.i(float(single, I::F32Sqrt, I::F64Sqrt));
            }
            Opcode::Ceil => {
                self.get(arg)?;
                self.i(float(single, I::F32Ceil, I::F64Ceil));
            }
            Opcode::Floor => {
                self.get(arg)?;
                self.i(float(single, I::F32Floor, I::F64Floor));
            }
            Opcode::Trunc => {
                self.get(arg)?;
                self.i(float(single, I::F32Trunc, I::F64Trunc));
            }
            Opcode::Nearest => {
                self.get(arg)?;
                self.i(float(single, I::F32Nearest, I::F64Nearest));
            }
            Opcode::Fpromote => {
                self.get(arg)?;
                self.i(I::F64PromoteF32);
            }
            Opcode::Fdemote => {
                self.get(arg)?;
                self.i(I::F32DemoteF64);
            }
            Opcode::FcvtFromUint | Opcode::FcvtFromSint => {
                let signed = opcode == Opcode::FcvtFromSint;
                self.get_for(arg, signed)?;
                self.i(match (from, to, signed) {
                    (Class::Word(_), Class::F32, false) => I::F32ConvertI32U,
                    (Class::Word(_), Class::F32, true) => I::F32ConvertI32S,
                    (Class::Word(_), Class::F64, false) => I::F64ConvertI32U,
                    (Class::Word(_), Class::F64, true) => I::F64ConvertI32S,
                    (Class::Long, Class::F32, false) => I::F32ConvertI64U,
                    (Class::Long, Class::F32, true) => I::F32ConvertI64S,
                    (Class::Long, Class::F64, false) => I::F64ConvertI64U,
                    (Class::Long, Class::F64, true) => I::F64ConvertI64S,
                    // alloc: the error's message
                    _ => return Err(format!("wasm: {opcode} from {from:?}")),
                });
            }
            Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => {
                let signed = opcode == Opcode::FcvtToSintSat;
                self.get(arg)?;
                self.i(match (single, to, signed) {
                    (true, Class::Word(_), false) => I::I32TruncSatF32U,
                    (true, Class::Word(_), true) => I::I32TruncSatF32S,
                    (false, Class::Word(_), false) => I::I32TruncSatF64U,
                    (false, Class::Word(_), true) => I::I32TruncSatF64S,
                    (true, Class::Long, false) => I::I64TruncSatF32U,
                    (true, Class::Long, true) => I::I64TruncSatF32S,
                    (false, Class::Long, false) => I::I64TruncSatF64U,
                    (false, Class::Long, true) => I::I64TruncSatF64S,
                    // alloc: the error's message
                    _ => return Err(format!("wasm: {opcode} to {to:?}")),
                });
                if let Some(bits) = to.narrow() {
                    // Saturated to 32 bits so far: the narrow type has bounds of its own.
                    let (low, high) = match signed {
                        true => (-(1i32 << (bits - 1)), (1i32 << (bits - 1)) - 1),
                        false => (0, (1i32 << bits) - 1),
                    };
                    let value = self.temp(ValType::I32);
                    for (bound, keep) in [(high, Op::LtS), (low, Op::GtS)] {
                        let keep = if signed { keep } else { keep.strict_unsigned() };
                        self.i(I::LocalTee(value));
                        self.i(I::I32Const(bound));
                        self.i(I::LocalGet(value));
                        self.i(I::I32Const(bound));
                        self.op(keep, false);
                        self.i(I::Select);
                    }
                    self.mask(bits);
                }
            }
            // alloc: the error's message
            _ => return Err(format!("wasm: {opcode}")),
        }
        self.set(result)
    }

    /// Reverses the bytes of the integer on the stack.
    fn swap_bytes(&mut self, long: bool, bits: u8) {
        let value = self.temp(if long { ValType::I64 } else { ValType::I32 });
        // Neighbouring bytes change places, then neighbouring pairs of them, and so on.
        let steps: &[(i64, u64)] = match bits {
            16 => &[(8, 0x00ff)],
            32 => &[(8, 0x00ff_00ff), (16, 0x0000_ffff)],
            _ => &[(8, 0x00ff_00ff_00ff_00ff), (16, 0x0000_ffff_0000_ffff), (32, 0xffff_ffff)],
        };
        self.swap_groups(value, long, steps);
    }

    /// Reverses the bits of the integer on the stack, which is `bits` wide.
    fn reverse_bits(&mut self, long: bool, bits: u8) {
        let value = self.temp(if long { ValType::I64 } else { ValType::I32 });
        let ones = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let steps = [
            (1, 0x5555_5555_5555_5555 & ones),
            (2, 0x3333_3333_3333_3333 & ones),
            (4, 0x0f0f_0f0f_0f0f_0f0f & ones),
        ];
        self.swap_groups(value, long, &steps);
        if bits > 8 {
            self.swap_bytes(long, bits);
        }
    }

    /// For each step, swaps every group of `shift` bits with the one beside it: `mask` has
    /// the lower group of each pair set.
    fn swap_groups(&mut self, value: u32, long: bool, steps: &[(i64, u64)]) {
        for &(shift, mask) in steps {
            self.i(I::LocalTee(value));
            self.constant(mask as i64, long);
            self.op(Op::And, long);
            self.constant(shift, long);
            self.op(Op::Shl, long);
            self.i(I::LocalGet(value));
            self.constant(shift, long);
            self.op(Op::ShrU, long);
            self.constant(mask as i64, long);
            self.op(Op::And, long);
            self.op(Op::Or, long);
        }
    }

    fn binary(
        &mut self,
        opcode: Opcode,
        results: &[Value],
        a: Value,
        b: Value,
    ) -> Result<(), String> {
        let result = results[0];
        let class = self.class(a)?;
        if opcode == Opcode::Iconcat {
            self.get(a)?;
            self.get(b)?;
            return self.set(result);
        }
        match class {
            Class::Word(_) | Class::Long => self.binary_int(opcode, results, a, b, class),
            Class::Pair => self.binary_wide(opcode, results, a, b),
            Class::V128 => self.binary_vector(opcode, result, a, b),
            Class::F32 | Class::F64 => {
                let single = class == Class::F32;
                let float = |f32: I<'static>, f64: I<'static>| if single { f32 } else { f64 };
                let instruction = match opcode {
                    Opcode::Fadd => float(I::F32Add, I::F64Add),
                    Opcode::Fsub => float(I::F32Sub, I::F64Sub),
                    Opcode::Fmul => float(I::F32Mul, I::F64Mul),
                    Opcode::Fdiv => float(I::F32Div, I::F64Div),
                    Opcode::Fmin => float(I::F32Min, I::F64Min),
                    Opcode::Fmax => float(I::F32Max, I::F64Max),
                    Opcode::Fcopysign => float(I::F32Copysign, I::F64Copysign),
                    // alloc: the error's message
                    _ => return Err(format!("wasm: {opcode}")),
                };
                self.get(a)?;
                self.get(b)?;
                self.i(instruction);
                self.set(result)
            }
        }
    }

    fn binary_int(
        &mut self,
        opcode: Opcode,
        results: &[Value],
        a: Value,
        b: Value,
        class: Class,
    ) -> Result<(), String> {
        let result = results[0];
        let long = class.is_long();
        let bits = match class {
            Class::Word(bits) => bits,
            _ => 64,
        };
        let simple = match opcode {
            Opcode::Iadd => Some(Op::Add),
            Opcode::Isub => Some(Op::Sub),
            Opcode::Imul => Some(Op::Mul),
            Opcode::Band => Some(Op::And),
            Opcode::Bor => Some(Op::Or),
            Opcode::Bxor => Some(Op::Xor),
            Opcode::Udiv => Some(Op::DivU),
            Opcode::Urem => Some(Op::RemU),
            Opcode::Sdiv => Some(Op::DivS),
            Opcode::Srem => Some(Op::RemS),
            _ => None,
        };
        if let Some(op) = simple {
            self.get_for(a, op.is_signed())?;
            self.get_for(b, op.is_signed())?;
            self.op(op, long);
            self.mask_for(class);
            return self.set(result);
        }
        match opcode {
            Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
                let op = match opcode {
                    Opcode::Ishl => Op::Shl,
                    Opcode::Ushr => Op::ShrU,
                    _ => Op::ShrS,
                };
                self.get_for(a, op.is_signed())?;
                self.amount(b, long)?;
                if bits < 32 {
                    // Wasm takes the amount modulo 32, the narrow type modulo its width.
                    self.i(I::I32Const(i32::from(bits) - 1));
                    self.i(I::I32And);
                }
                self.op(op, long);
                self.mask_for(class);
            }
            Opcode::Rotl | Opcode::Rotr => {
                let left = opcode == Opcode::Rotl;
                if bits >= 32 {
                    self.get(a)?;
                    self.amount(b, long)?;
                    self.op(if left { Op::Rotl } else { Op::Rotr }, long);
                } else {
                    // Two shifts that meet: the second amount is what is left of the width.
                    let amount = self.temp(ValType::I32);
                    let (there, back) =
                        if left { (Op::Shl, Op::ShrU) } else { (Op::ShrU, Op::Shl) };
                    self.amount(b, false)?;
                    self.i(I::I32Const(i32::from(bits) - 1));
                    self.i(I::I32And);
                    self.i(I::LocalSet(amount));
                    self.get(a)?;
                    self.i(I::LocalGet(amount));
                    self.op(there, false);
                    self.get(a)?;
                    self.i(I::I32Const(i32::from(bits)));
                    self.i(I::LocalGet(amount));
                    self.i(I::I32Sub);
                    self.i(I::I32Const(i32::from(bits) - 1));
                    self.i(I::I32And);
                    self.op(back, false);
                    self.i(I::I32Or);
                    self.mask(bits);
                }
            }
            Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax => {
                let pick = match opcode {
                    Opcode::Umin => Op::LtU,
                    Opcode::Umax => Op::GtU,
                    Opcode::Smin => Op::LtS,
                    _ => Op::GtS,
                };
                self.get(a)?;
                self.get(b)?;
                self.compare(pick, a, b)?;
                self.i(I::Select);
            }
            Opcode::Umulhi | Opcode::Smulhi if long => {
                let (x, y) = (self.local(a)?, self.local(b)?);
                self.multiply_high(x, y, opcode == Opcode::Smulhi);
            }
            Opcode::UaddOverflow | Opcode::UsubOverflow => {
                let add = opcode == Opcode::UaddOverflow;
                self.get(a)?;
                self.get(b)?;
                self.op(if add { Op::Add } else { Op::Sub }, long);
                self.mask_for(class);
                self.set(result)?;
                // A sum that wrapped is below what went in, and a difference wraps when
                // more is taken than there is.
                self.get(if add { result } else { a })?;
                self.get(if add { a } else { b })?;
                self.op(Op::LtU, long);
                return self.set(results[1]);
            }
            Opcode::SaddOverflow | Opcode::SsubOverflow if long => {
                let add = opcode == Opcode::SaddOverflow;
                self.get(a)?;
                self.get(b)?;
                self.op(if add { Op::Add } else { Op::Sub }, true);
                self.set(result)?;
                // The sign is wrong when both operands differ from the sum in it, and for a
                // difference when the operands differ and the result is not on `a`'s side.
                self.get(a)?;
                self.get(if add { result } else { b })?;
                self.i(I::I64Xor);
                self.get(if add { b } else { a })?;
                self.get(result)?;
                self.i(I::I64Xor);
                self.i(I::I64And);
                self.i(I::I64Const(0));
                self.i(I::I64LtS);
                return self.set(results[1]);
            }
            Opcode::SaddOverflow | Opcode::SsubOverflow | Opcode::SmulOverflow if !long => {
                // In 64 bits nothing overflows: the result fits the type when extending its
                // low bits gives it back.
                let exact = self.temp(ValType::I64);
                for value in [a, b] {
                    self.get_signed(value)?;
                    self.i(I::I64ExtendI32S);
                }
                self.i(match opcode {
                    Opcode::SaddOverflow => I::I64Add,
                    Opcode::SsubOverflow => I::I64Sub,
                    _ => I::I64Mul,
                });
                self.i(I::LocalTee(exact));
                self.i(I::I32WrapI64);
                self.mask_for(class);
                self.set(result)?;
                self.i(I::LocalGet(exact));
                self.i(I::LocalGet(exact));
                self.i(match bits {
                    8 => I::I64Extend8S,
                    16 => I::I64Extend16S,
                    _ => I::I64Extend32S,
                });
                self.i(I::I64Ne);
                return self.set(results[1]);
            }
            Opcode::UmulOverflow if !long => {
                let exact = self.temp(ValType::I64);
                for value in [a, b] {
                    self.get(value)?;
                    self.i(I::I64ExtendI32U);
                }
                self.i(I::I64Mul);
                self.i(I::LocalTee(exact));
                self.i(I::I32WrapI64);
                self.mask_for(class);
                self.set(result)?;
                self.i(I::LocalGet(exact));
                self.i(I::I64Const(i64::from(bits)));
                self.i(I::I64ShrU);
                self.i(I::I64Const(0));
                self.i(I::I64Ne);
                return self.set(results[1]);
            }
            Opcode::UmulOverflow | Opcode::SmulOverflow => {
                let signed = opcode == Opcode::SmulOverflow;
                let (x, y) = (self.local(a)?, self.local(b)?);
                self.get(a)?;
                self.get(b)?;
                self.i(I::I64Mul);
                self.set(result)?;
                // The upper half of the full product has to be nothing but the sign of the
                // lower one.
                self.multiply_high(x, y, signed);
                if signed {
                    self.get(result)?;
                    self.i(I::I64Const(63));
                    self.i(I::I64ShrS);
                } else {
                    self.i(I::I64Const(0));
                }
                self.i(I::I64Ne);
                return self.set(results[1]);
            }
            // alloc: the error's message
            _ => return Err(format!("wasm: {opcode}")),
        }
        self.set(result)
    }

    /// Pushes the upper 64 bits of the product of two `i64` locals.
    fn multiply_high(&mut self, a: u32, b: u32, signed: bool) {
        let [low, cross] = [(); 2].map(|()| self.temp(ValType::I64));
        let half = |this: &mut Self, local: u32, upper: bool| {
            this.i(I::LocalGet(local));
            if upper {
                this.i(I::I64Const(32));
                this.i(I::I64ShrU);
            } else {
                this.i(I::I64Const(0xffff_ffff));
                this.i(I::I64And);
            }
        };
        // Schoolbook multiplication on 32-bit halves, carrying through the middle.
        half(self, a, true);
        half(self, b, false);
        self.i(I::I64Mul);
        half(self, a, false);
        half(self, b, false);
        self.i(I::I64Mul);
        self.i(I::I64Const(32));
        self.i(I::I64ShrU);
        self.i(I::I64Add);
        self.i(I::LocalSet(cross));
        half(self, a, false);
        half(self, b, true);
        self.i(I::I64Mul);
        half(self, cross, false);
        self.i(I::I64Add);
        self.i(I::LocalSet(low));
        half(self, a, true);
        half(self, b, true);
        self.i(I::I64Mul);
        half(self, cross, true);
        self.i(I::I64Add);
        half(self, low, true);
        self.i(I::I64Add);
        if signed {
            // A negative operand counted as 2^64 more than it is, times the other.
            for (negative, other) in [(a, b), (b, a)] {
                self.i(I::LocalGet(other));
                self.i(I::I64Const(0));
                self.i(I::LocalGet(negative));
                self.i(I::I64Const(0));
                self.i(I::I64LtS);
                self.i(I::Select);
                self.i(I::I64Sub);
            }
        }
    }

    fn binary_wide(
        &mut self,
        opcode: Opcode,
        results: &[Value],
        a: Value,
        b: Value,
    ) -> Result<(), String> {
        let result = results[0];
        let out = self.local(result)?;
        match opcode {
            Opcode::Band | Opcode::Bor | Opcode::Bxor => {
                let op = match opcode {
                    Opcode::Band => Op::And,
                    Opcode::Bor => Op::Or,
                    _ => Op::Xor,
                };
                for high in [false, true] {
                    if high {
                        self.get_high(a)?
                    } else {
                        self.get_low(a)?
                    }
                    if high {
                        self.get_high(b)?
                    } else {
                        self.get_low(b)?
                    }
                    self.op(op, true);
                }
                self.set(result)
            }
            Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax => {
                let pick = match opcode {
                    Opcode::Umin => Op::LtU,
                    Opcode::Umax => Op::GtU,
                    Opcode::Smin => Op::LtS,
                    _ => Op::GtS,
                };
                let first = self.temp(ValType::I32);
                self.compare(pick, a, b)?;
                self.i(I::LocalSet(first));
                for half in [0, 1] {
                    let (x, y) = (self.local(a)? + half, self.local(b)? + half);
                    self.i(I::LocalGet(x));
                    self.i(I::LocalGet(y));
                    self.i(I::LocalGet(first));
                    self.i(I::Select);
                    self.i(I::LocalSet(out + half));
                }
                Ok(())
            }
            Opcode::Rotl | Opcode::Rotr => {
                // A rotation right is one left by what is left of the width.
                let amount = self.temp(ValType::I64);
                let [low, high] = [(); 2].map(|()| self.temp(ValType::I64));
                if opcode == Opcode::Rotr {
                    self.i(I::I64Const(0));
                }
                self.amount(b, true)?;
                if opcode == Opcode::Rotr {
                    self.i(I::I64Sub);
                }
                self.i(I::I64Const(127));
                self.i(I::I64And);
                self.i(I::LocalSet(amount));
                // Past 64 the halves change places first.
                let x = self.local(a)?;
                for (local, (far, near)) in [(low, (x + 1, x)), (high, (x, x + 1))] {
                    self.i(I::LocalGet(far));
                    self.i(I::LocalGet(near));
                    self.i(I::LocalGet(amount));
                    self.i(I::I64Const(64));
                    self.i(I::I64GeU);
                    self.i(I::Select);
                    self.i(I::LocalSet(local));
                }
                // Each half takes what leaves the other, in two steps so that an amount of
                // zero brings nothing over.
                for (into, (this, other)) in [(out, (low, high)), (out + 1, (high, low))] {
                    self.i(I::LocalGet(this));
                    self.i(I::LocalGet(amount));
                    self.i(I::I64Shl);
                    self.i(I::LocalGet(other));
                    self.i(I::I64Const(1));
                    self.i(I::I64ShrU);
                    self.i(I::I64Const(63));
                    self.i(I::LocalGet(amount));
                    self.i(I::I64Sub);
                    self.i(I::I64ShrU);
                    self.i(I::I64Or);
                    self.i(I::LocalSet(into));
                }
                Ok(())
            }
            Opcode::Iadd
            | Opcode::Isub
            | Opcode::UaddOverflow
            | Opcode::UsubOverflow
            | Opcode::SaddOverflow
            | Opcode::SsubOverflow => {
                let add =
                    matches!(opcode, Opcode::Iadd | Opcode::UaddOverflow | Opcode::SaddOverflow);
                let op = if add { Op::Add } else { Op::Sub };
                let [low, high] = [(); 2].map(|()| self.temp(ValType::I64));
                self.get_low(a)?;
                self.get_low(b)?;
                self.op(op, true);
                self.i(I::LocalSet(low));
                self.get_high(a)?;
                self.get_high(b)?;
                self.op(op, true);
                // What the low halves carry or borrow.
                if add {
                    self.i(I::LocalGet(low));
                    self.get_low(a)?;
                } else {
                    self.get_low(a)?;
                    self.get_low(b)?;
                }
                self.i(I::I64LtU);
                self.i(I::I64ExtendI32U);
                self.op(op, true);
                self.i(I::LocalSet(high));
                match opcode {
                    Opcode::UaddOverflow | Opcode::UsubOverflow => {
                        // As for one word, over both: the high halves decide unless equal.
                        let (x_low, x_high, y) = if add { (low, high, a) } else { (0, 0, b) };
                        if add {
                            self.i(I::LocalGet(x_low));
                            self.get_low(y)?;
                            self.i(I::I64LtU);
                            self.i(I::LocalGet(x_high));
                            self.get_high(y)?;
                            self.i(I::I64LtU);
                            self.i(I::LocalGet(x_high));
                            self.get_high(y)?;
                            self.i(I::I64Eq);
                            self.i(I::Select);
                        } else {
                            self.compare(Op::LtU, a, y)?;
                        }
                        self.set(results[1])?;
                    }
                    Opcode::SaddOverflow | Opcode::SsubOverflow => {
                        self.get_high(a)?;
                        if add {
                            self.i(I::LocalGet(high));
                        } else {
                            self.get_high(b)?;
                        }
                        self.i(I::I64Xor);
                        if add {
                            self.get_high(b)?;
                        } else {
                            self.get_high(a)?;
                        }
                        self.i(I::LocalGet(high));
                        self.i(I::I64Xor);
                        self.i(I::I64And);
                        self.i(I::I64Const(0));
                        self.i(I::I64LtS);
                        self.set(results[1])?;
                    }
                    _ => {}
                }
                self.i(I::LocalGet(low));
                self.i(I::LocalSet(out));
                self.i(I::LocalGet(high));
                self.i(I::LocalSet(out + 1));
                Ok(())
            }
            Opcode::Imul => {
                let (x, y) = (self.local(a)?, self.local(b)?);
                let [low, high] = [(); 2].map(|()| self.temp(ValType::I64));
                self.i(I::LocalGet(x));
                self.i(I::LocalGet(y));
                self.i(I::I64Mul);
                self.i(I::LocalSet(low));
                self.multiply_high(x, y, false);
                self.i(I::LocalGet(x));
                self.i(I::LocalGet(y + 1));
                self.i(I::I64Mul);
                self.i(I::I64Add);
                self.i(I::LocalGet(x + 1));
                self.i(I::LocalGet(y));
                self.i(I::I64Mul);
                self.i(I::I64Add);
                self.i(I::LocalSet(high));
                self.i(I::LocalGet(low));
                self.i(I::LocalSet(out));
                self.i(I::LocalGet(high));
                self.i(I::LocalSet(out + 1));
                Ok(())
            }
            Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
                let x = self.local(a)?;
                let amount = self.temp(ValType::I64);
                let [low, high] = [(); 2].map(|()| self.temp(ValType::I64));
                self.amount(b, true)?;
                self.i(I::I64Const(127));
                self.i(I::I64And);
                self.i(I::LocalSet(amount));
                // `near` is the half the bits leave and `far` the one they arrive in.
                let left = opcode == Opcode::Ishl;
                let signed = opcode == Opcode::Sshr;
                let (near, far, near_out, far_out) =
                    if left { (x, x + 1, low, high) } else { (x + 1, x, high, low) };
                let (there, back) = if left { (Op::Shl, Op::ShrU) } else { (Op::ShrU, Op::Shl) };
                let near_shift = if signed { Op::ShrS } else { there };
                self.i(I::LocalGet(amount));
                self.i(I::I64Const(64));
                self.i(I::I64GeU);
                self.i(I::If(BlockType::Empty));
                {
                    // Everything crosses over: wasm takes the amount modulo 64 itself.
                    self.i(I::LocalGet(near));
                    self.i(I::LocalGet(amount));
                    self.op(near_shift, true);
                    self.i(I::LocalSet(far_out));
                    if signed {
                        self.i(I::LocalGet(near));
                        self.i(I::I64Const(63));
                        self.i(I::I64ShrS);
                    } else {
                        self.i(I::I64Const(0));
                    }
                    self.i(I::LocalSet(near_out));
                }
                self.i(I::Else);
                {
                    self.i(I::LocalGet(near));
                    self.i(I::LocalGet(amount));
                    self.op(near_shift, true);
                    self.i(I::LocalSet(near_out));
                    self.i(I::LocalGet(far));
                    self.i(I::LocalGet(amount));
                    self.op(there, true);
                    // What crosses over, in two steps so that an amount of zero moves
                    // nothing rather than everything.
                    self.i(I::LocalGet(near));
                    self.i(I::I64Const(1));
                    self.op(back, true);
                    self.i(I::I64Const(63));
                    self.i(I::LocalGet(amount));
                    self.i(I::I64Sub);
                    self.op(back, true);
                    self.i(I::I64Or);
                    self.i(I::LocalSet(far_out));
                }
                self.i(I::End);
                self.i(I::LocalGet(low));
                self.i(I::LocalSet(out));
                self.i(I::LocalGet(high));
                self.i(I::LocalSet(out + 1));
                Ok(())
            }
            // alloc: the error's message
            _ => Err(format!("wasm: {opcode} of a wide integer")),
        }
    }
}

impl Op {
    fn strict_unsigned_or_equal(self) -> Op {
        match self {
            Op::LeS | Op::LeU => Op::LeU,
            _ => Op::GeU,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `WasmFunction::encode` wrote before the linker's format: a 4-byte length, then
    /// the header as postcard, then the body. The reference the fields are checked against.
    fn old_encode(function: &WasmFunction) -> (Vec<u8>, u32) {
        let header = WasmFunction {
            signature: function.signature.clone(),
            types: function.types.clone(),
            type_fixups: function.type_fixups.clone(),
            stack_pointer_fixups: function.stack_pointer_fixups.clone(),
            body: Vec::new(),
        };
        let header = postcard::to_allocvec(&header).unwrap();
        let mut bytes = (header.len() as u32).to_le_bytes().to_vec();
        bytes.extend(header);
        let start = bytes.len() as u32;
        bytes.extend(&function.body);
        (bytes, start)
    }

    fn old_decode(bytes: &[u8]) -> (WasmFunction, usize) {
        let start = 4 + u32::from_le_bytes(*bytes.first_chunk::<4>().unwrap()) as usize;
        let mut function: WasmFunction = postcard::from_bytes(&bytes[4..start]).unwrap();
        function.body = bytes[start..].to_vec();
        (function, start)
    }

    fn function() -> WasmFunction {
        let pair = WasmSignature { params: vec![WasmType::I64, WasmType::I64], results: vec![] };
        WasmFunction {
            signature: WasmSignature {
                params: vec![WasmType::I32, WasmType::I64, WasmType::F32, WasmType::F64, WasmType::V128],
                results: vec![WasmType::I32],
            },
            types: vec![pair, WasmSignature { params: vec![], results: vec![WasmType::F64] }],
            type_fixups: vec![(7, 1), (300, 0)],
            stack_pointer_fixups: vec![1, 200],
            // One local declaration, `global.get` of a padded index, `call_indirect`, `end`.
            body: vec![1, 2, 0x7f, 0x23, 0x80, 0x80, 0x80, 0x80, 0x00, 0x11, 0x81, 0x80, 0x80, 0x80, 0x00, 0x00, 0x0b],
        }
    }

    fn same(a: &WasmFunction, b: &WasmFunction) {
        assert_eq!(a.signature, b.signature);
        assert_eq!(a.types, b.types);
        assert_eq!(a.type_fixups, b.type_fixups);
        assert_eq!(a.stack_pointer_fixups, b.stack_pointer_fixups);
        assert_eq!(a.body, b.body);
    }

    #[test]
    fn a_function_is_the_linkers_code_and_holds_what_the_old_header_did() {
        let function = function();
        let (bytes, start) = function.encode().unwrap();
        assert_eq!((bytes.clone(), start), function.code().encode(&function.body));
        let (code, at) = format::Code::decode(&bytes).unwrap();
        assert_eq!(at, start as usize);
        assert_eq!(&bytes[at..], &function.body[..]);

        let (old_bytes, old_start) = old_encode(&function);
        let (old, _) = old_decode(&old_bytes);
        assert_eq!(code.ty, old.signature.format());
        assert_eq!(code.types, old.types.iter().map(WasmSignature::format).collect::<Vec<_>>());
        assert_eq!(code.type_fixups, old.type_fixups);
        assert_eq!(code.stack_pointer, old.stack_pointer_fixups);
        assert!(code.imports.is_empty() && code.export.is_none());

        let (back, back_start) = WasmFunction::decode(&bytes).unwrap();
        assert_eq!(back_start, start as usize);
        same(&back, &old);
        // A relocation counted from the start of the body lands on the same byte either way.
        for at in [0, 4, 10, function.body.len() - 1] {
            assert_eq!(bytes[start as usize + at], old_bytes[old_start as usize + at]);
        }
    }

    #[test]
    fn signatures_are_the_formats_value_types() {
        let signature = function().signature;
        let format = signature.format();
        let bytes: Vec<u8> = format.params.iter().map(|&it| it as u8).collect();
        assert_eq!(bytes, signature.params.iter().map(|&it| it as u8).collect::<Vec<_>>());
        assert_eq!(WasmSignature::of_format(&format).unwrap(), signature);
        let funcref = format::Signature { params: vec![format::ValType::FuncRef], results: vec![] };
        assert!(WasmSignature::of_format(&funcref).is_err());
    }

    #[test]
    fn a_header_with_an_export_is_not_a_compiled_function() {
        let function = function();
        let code = format::Code { export: Some("f".to_owned()), ..function.code() };
        assert!(WasmFunction::decode(&code.encode(&function.body).0).is_err());
        assert!(WasmFunction::decode(&function.encode().unwrap().0[..3]).is_err());
    }
}
