//! Cranelift IR of one function to LLVM IR.
//!
//! The IR arrives after ABI lowering: every parameter is a scalar or a pointer, every local is
//! a stack slot and every address is an `i64`. Blocks become blocks, block parameters become
//! phis, stack slots become allocas. What is an address is a pointer ([`addresses`],
//! [`pointerish`]), so that LLVM can tell what each access is based on, and parameters carry
//! what their Rust types say of the memory they point to ([`Pointee`]).

use cranelift_codegen::ir::{
    self, ArgumentExtension, ArgumentPurpose, AtomicRmwOp, Block, BlockArg, BlockCall,
    ExceptionTableItem, ExternalName, GlobalValueData, Inst, InstructionData, LibCall, Opcode,
    Signature, Type, Value,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::isa::CallConv;
use std::num::NonZeroU32;

use inkwell::{
    AddressSpace, AtomicOrdering, AtomicRMWBinOp, FloatPredicate, GlobalVisibility, IntPredicate,
    ThreadLocalMode,
    attributes::{Attribute, AttributeLoc},
    basic_block::BasicBlock,
    builder::{Builder, BuilderError},
    context::Context,
    debug_info::{
        AsDIScope, DICompileUnit, DIFile, DIFlags, DIFlagsConstants, DISubprogram,
        DWARFEmissionKind, DWARFSourceLanguage, DebugInfoBuilder, debug_metadata_version,
    },
    intrinsics::Intrinsic,
    module::{FlagBehavior, Linkage, Module},
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, IntType, VectorType},
    values::{
        BasicMetadataValueEnum, BasicValue, BasicValueEnum, CallSiteValue, FloatValue,
        FunctionValue, InstructionValue, IntValue, PhiValue, PointerValue, VectorValue,
    },
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use hir_def::hir::AsmOptions;
use inkwell::InlineAsmDialect;

use super::Unhandled;
use crate::{
    ObjectFormat, asm,
    fragment::Pointee,
    unwind::{self, seh::ARRIVAL},
};

type Lowered<T> = Result<T, Unhandled>;

/// A Cranelift value in LLVM.
#[derive(Clone, Copy)]
enum Operand<'ctx> {
    Int(IntValue<'ctx>),
    /// An address LLVM knows the origin of.
    Pointer(PointerValue<'ctx>),
    Float(FloatValue<'ctx>),
    Vector(VectorValue<'ctx>),
}

/// The library function a call is of, when it is one that copies or fills memory.
fn memory_call(name: &ExternalName) -> Option<LibCall> {
    match name {
        ExternalName::LibCall(libcall @ (LibCall::Memcpy | LibCall::Memmove | LibCall::Memset)) => {
            Some(*libcall)
        }
        _ => None,
    }
}

impl<'ctx> From<BasicValueEnum<'ctx>> for Operand<'ctx> {
    fn from(value: BasicValueEnum<'ctx>) -> Self {
        match value {
            BasicValueEnum::IntValue(it) => Operand::Int(it),
            BasicValueEnum::PointerValue(it) => Operand::Pointer(it),
            BasicValueEnum::FloatValue(it) => Operand::Float(it),
            BasicValueEnum::VectorValue(it) => Operand::Vector(it),
            other => unreachable!("no Cranelift value is {other:?}"),
        }
    }
}

/// Which end of a call a signature is looked at from.
#[derive(Clone, Copy)]
enum Side {
    Caller,
    Callee,
}

/// What an unwinding call lands on.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Pad {
    handler: Block,
    tag: u32,
}

pub(super) struct Lowering<'ctx, 'f> {
    context: &'ctx Context,
    module: &'f Module<'ctx>,
    builder: Builder<'ctx>,
    func: &'f ir::Function,
    function: FunctionValue<'ctx>,
    values: FxHashMap<Value, Operand<'ctx>>,
    /// The `i1` a comparison gave, for the branch or select that uses it.
    conditions: FxHashMap<Value, IntValue<'ctx>>,
    blocks: FxHashMap<Block, BasicBlock<'ctx>>,
    phis: FxHashMap<Block, Vec<PhiValue<'ctx>>>,
    slots: FxHashMap<ir::StackSlot, PointerValue<'ctx>>,
    callees: FxHashMap<ir::FuncRef, FunctionValue<'ctx>>,
    globals: FxHashMap<ir::GlobalValue, PointerValue<'ctx>>,
    /// The bytes of the data objects that a load from is known to give, by the number in
    /// their names.
    constants: &'f FxHashMap<u32, &'f [u8]>,
    /// The numbers in the names of the data objects that are statics.
    statics: &'f FxHashSet<u32>,
    /// The number in the name of `_Unwind_Resume`, whose calls are LLVM's `resume`.
    resume: Option<u32>,
    /// The number in the name of `memcmp`, which is called by its own name: LLVM writes a
    /// comparison of the C library's of a few bytes it knows the number of as loads.
    memcmp: Option<u32>,
    /// The `asm!` blocks whose wrappers are called, by the numbers in the wrappers' names, that
    /// go where they are called as LLVM's inline assembly.
    blocks_asm: &'f FxHashMap<u32, Arc<asm::Inline>>,
    pads: FxHashMap<Pad, BasicBlock<'ctx>>,
    /// The block the allocas are in.
    frame: Option<BasicBlock<'ctx>>,
    /// Where a guarded call that unwound leaves its exception (see `unwind::seh`).
    arrival: Option<PointerValue<'ctx>>,
    /// What the line table LLVM writes is told through, where one is read back.
    marks: Option<Marks<'ctx, 'f>>,
    /// How often each branch went each way while code of the function ran.
    weights: Option<&'f crate::tier::Weights>,
    /// The values that are addresses, which are pointers in LLVM: see [`addresses`].
    addresses: FxHashSet<Value>,
    /// The parameters of each callee, by its name, that are pointers: see
    /// [`Lowering::pointer_params`].
    pointer_params: FxHashMap<String, Vec<u32>>,
    /// The values that are or may be addresses: see [`pointerish`]. A block parameter of
    /// one is a phi of pointers.
    pointerish: FxHashSet<Value>,
    /// The functions called whose bodies LLVM is shown, by their place among the requests.
    shown: &'f FxHashMap<u32, FunctionValue<'ctx>>,
    /// The addresses rounded up in slots aligned beyond what Cranelift aligns: see
    /// [`realigned`].
    realigned: FxHashMap<Inst, (ir::StackSlot, u32)>,
}

/// Each `(address + align - 1) & -align` of a stack slot, with the slot and the alignment:
/// Cranelift aligns a frame by 16 bytes, so a slot that needs more is larger by as much and
/// the address rounded up. LLVM aligns an alloca by any power of two, and the rounded address
/// is then the alloca's, which LLVM can follow where it can't through the arithmetic.
fn realigned(func: &ir::Function) -> FxHashMap<Inst, (ir::StackSlot, u32)> {
    let dfg = &func.dfg;
    let made = |value: Value| match dfg.value_def(dfg.resolve_aliases(value)) {
        ir::ValueDef::Result(inst, _) => Some(dfg.insts[inst]),
        _ => None,
    };
    let constant = |value: Value| match made(value)? {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm, .. } => Some(imm.bits()),
        _ => None,
    };
    let mut found = FxHashMap::default();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let InstructionData::Binary { opcode: Opcode::Band, args: [bumped, mask] } =
                dfg.insts[inst]
            else {
                continue;
            };
            let Some(align) = constant(mask).map(i64::wrapping_neg) else { continue };
            if align <= 16 || align > 1 << 16 || align.count_ones() != 1 {
                continue;
            }
            let Some(InstructionData::Binary { opcode: Opcode::Iadd, args: [base, by] }) =
                made(bumped)
            else {
                continue;
            };
            let Some(InstructionData::StackAddr { stack_slot, offset, .. }) = made(base) else {
                continue;
            };
            let room = func.sized_stack_slots[stack_slot].size;
            if constant(by) == Some(align - 1) && i32::from(offset) == 0 && room >= align as u32 {
                found.insert(inst, (stack_slot, align as u32));
            }
        }
    }
    found
}

/// Tells LLVM that `function` was entered `count` times while code of it counted.
pub(super) fn entry_count<'ctx>(context: &'ctx Context, function: FunctionValue<'ctx>, count: u64) {
    let count = context.i64_type().const_int(count, false);
    let node = context
        .metadata_node(&[context.metadata_string("function_entry_count").into(), count.into()]);
    use inkwell::{llvm_sys::core, values::AsValueRef as _};
    // SAFETY: both are of this module's context, and a function is a global.
    unsafe {
        let node = core::LLVMValueAsMetadata(node.as_value_ref());
        core::LLVMGlobalSetMetadata(function.as_value_ref(), context.get_kind_id("prof"), node);
    }
}

/// A function of the module and what it is lowered from.
pub(super) struct Defined<'f> {
    pub(super) name: &'f str,
    pub(super) func: &'f ir::Function,
    pub(super) pointees: &'f [(u32, Pointee)],
    /// The slots of locals with a name, whose places are told.
    pub(super) named: &'f [ir::StackSlot],
    pub(super) weights: Option<&'f crate::tier::Weights>,
    /// Its line table and variables are told: it is the function being compiled.
    pub(super) marked: bool,
}

/// How values move in a function without being computed: what each block parameter is
/// passed, and what is stored at and loaded from each place of a stack slot.
struct Flow {
    /// What each block parameter is passed; a call's result passed is the parameter itself.
    incoming: FxHashMap<Value, Vec<Value>>,
    /// The values of pointer width stored at each place, and the place each load of pointer
    /// width reads.
    stored: FxHashMap<(ir::StackSlot, i64), Vec<Value>>,
    loaded: FxHashMap<Value, (ir::StackSlot, i64)>,
}

impl Flow {
    fn of(func: &ir::Function) -> Flow {
        let dfg = &func.dfg;
        let constant = |value: Value| match dfg.value_def(dfg.resolve_aliases(value)) {
            ir::ValueDef::Result(inst, _) => match dfg.insts[inst] {
                InstructionData::UnaryImm { opcode: Opcode::Iconst, imm, .. } => Some(imm.bits()),
                _ => None,
            },
            _ => None,
        };
        // The place of a slot `address` is, through the constants added to its address.
        let place = |address: Value, offset: i32| {
            let (mut address, mut offset) = (address, i64::from(offset));
            loop {
                let ir::ValueDef::Result(inst, _) = dfg.value_def(dfg.resolve_aliases(address))
                else {
                    return None;
                };
                match dfg.insts[inst] {
                    InstructionData::StackAddr { stack_slot, offset: at, .. } => {
                        return Some((stack_slot, i64::from(i32::from(at)) + offset));
                    }
                    InstructionData::Binary { opcode: Opcode::Iadd, args: [a, b] } => {
                        let (base, by) = match (constant(a), constant(b)) {
                            (_, Some(by)) => (a, by),
                            (Some(by), _) => (b, by),
                            _ => return None,
                        };
                        address = base;
                        offset = offset.checked_add(by)?;
                    }
                    _ => return None,
                }
            }
        };
        let mut flow = Flow {
            incoming: FxHashMap::default(),
            stored: FxHashMap::default(),
            loaded: FxHashMap::default(),
        };
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let data = &dfg.insts[inst];
                match *data {
                    InstructionData::Store {
                        opcode: Opcode::Store,
                        args: [value, address],
                        offset,
                        ..
                    } if dfg.value_type(value) == types::I64 => {
                        if let Some(at) = place(address, offset.into()) {
                            flow.stored.entry(at).or_default().push(dfg.resolve_aliases(value));
                        }
                    }
                    InstructionData::Load { opcode: Opcode::Load, arg, offset, .. }
                        if dfg.value_type(dfg.first_result(inst)) == types::I64 =>
                    {
                        if let Some(at) = place(arg, offset.into()) {
                            flow.loaded.insert(dfg.first_result(inst), at);
                        }
                    }
                    _ => {}
                }
                for call in data.branch_destination(&dfg.jump_tables, &dfg.exception_tables) {
                    let target = call.block(&dfg.value_lists);
                    let params = dfg.block_params(target).iter();
                    for (&param, arg) in params.zip(call.args(&dfg.value_lists)) {
                        let passed = match arg {
                            BlockArg::Value(value) => dfg.resolve_aliases(value),
                            _ => param,
                        };
                        flow.incoming.entry(param).or_default().push(passed);
                    }
                }
            }
        }
        flow
    }

    /// Where `value` may come from without being computed: what is passed to it as a block
    /// parameter, or stored where it is loaded from.
    fn sources(&self, value: Value) -> &[Value] {
        if let Some(passed) = self.incoming.get(&value) {
            return passed;
        }
        match self.loaded.get(&value).and_then(|at| self.stored.get(at)) {
            Some(stored) => stored,
            None => &[],
        }
    }
}

/// The values of `func` that are used as addresses, and those they are made of: what a load,
/// a store or an atomic access goes to, the base of an addition that gives one, both values
/// a `select` of one picks from, and what is passed to a block parameter or stored in the
/// place of a slot that is one. A load of one gives a pointer, a block parameter of one is a
/// phi of pointers, and so on, so that LLVM sees which address each access is based on rather
/// than an integer turned into a pointer where it is used, which could be based on anything.
///
/// A wrong guess costs nothing but speed: a value that is used as an integer as well is
/// turned into one there (`ptrtoint`, which LLVM takes for the address escaping).
fn addresses(func: &ir::Function, flow: &Flow) -> FxHashSet<Value> {
    let dfg = &func.dfg;
    let mut work = Vec::new();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            match dfg.insts[inst] {
                InstructionData::Load { arg, .. } => work.push(arg),
                InstructionData::LoadNoOffset { opcode: Opcode::AtomicLoad, arg, .. } => {
                    work.push(arg)
                }
                InstructionData::Store { args: [_, address], .. }
                | InstructionData::StoreNoOffset { args: [_, address], .. }
                | InstructionData::AtomicRmw { args: [address, _], .. }
                | InstructionData::AtomicCas { args: [address, ..], .. } => work.push(address),
                _ => {}
            }
        }
    }
    // An offset is computed; a base is loaded, passed or the address of something.
    let offset = |value: Value| match dfg.value_def(value) {
        ir::ValueDef::Result(inst, _) => matches!(
            dfg.insts[inst].opcode(),
            Opcode::Iconst
                | Opcode::Imul
                | Opcode::Ishl
                | Opcode::Uextend
                | Opcode::Sextend
                | Opcode::Ineg
        ),
        _ => false,
    };
    let mut found = FxHashSet::default();
    while let Some(value) = work.pop() {
        let value = dfg.resolve_aliases(value);
        if dfg.value_type(value) != types::I64 || !found.insert(value) {
            continue;
        }
        work.extend(flow.sources(value).iter().copied());
        if let ir::ValueDef::Result(inst, _) = dfg.value_def(value) {
            match dfg.insts[inst] {
                InstructionData::Binary { opcode: Opcode::Iadd, args: [a, b] } => {
                    work.push(if offset(a) { b } else { a });
                }
                InstructionData::Ternary { opcode: Opcode::Select, args: [_, a, b] } => {
                    work.extend([a, b]);
                }
                _ => {}
            }
        }
    }
    found
}

/// The values of `func` that are or may be addresses: [`addresses`], the parameters that
/// are pointers, the addresses of slots, symbols and functions, what is added to one, and
/// what comes only from those (and constants) through block parameters and slots. A call
/// that passes one passes a pointer, so that it does not escape before the call.
fn pointerish(
    func: &ir::Function,
    flow: &Flow,
    addresses: &FxHashSet<Value>,
    params: &[Value],
) -> FxHashSet<Value> {
    let dfg = &func.dfg;
    let mut found = addresses.clone();
    found.extend(params.iter().copied());
    let constant = |value: Value| {
        matches!(dfg.value_def(value), ir::ValueDef::Result(inst, _)
            if dfg.insts[inst].opcode() == Opcode::Iconst)
    };
    let moved: Vec<Value> = flow.incoming.keys().chain(flow.loaded.keys()).copied().collect();
    loop {
        let before = found.len();
        for &value in &moved {
            let sources = flow.sources(value);
            if !found.contains(&value)
                && sources.iter().any(|it| found.contains(it))
                && sources.iter().all(|it| found.contains(it) || constant(*it) || *it == value)
            {
                found.insert(value);
            }
        }
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let is = match dfg.insts[inst] {
                    InstructionData::StackAddr { .. }
                    | InstructionData::UnaryGlobalValue { .. }
                    | InstructionData::FuncAddr { .. } => true,
                    InstructionData::Binary { opcode: Opcode::Iadd, args: [a, b] } => {
                        found.contains(&dfg.resolve_aliases(a))
                            || found.contains(&dfg.resolve_aliases(b))
                    }
                    _ => false,
                };
                if is {
                    found.insert(dfg.first_result(inst));
                }
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// The type of a static's global: one without a size, which LLVM does not take to be at an
/// address of its own.
const STATIC: &str = "blaze.static";

/// The source locations of the IR as lines of a file that is not there, for LLVM to say
/// in its line table which instructions became which code, and the stack slots of named
/// locals as variables, for it to say where it kept them. `emitted` reads both back.
/// `DW_ATE_unsigned`: the bytes of a slot are told to LLVM as one number.
const DW_ATE_UNSIGNED: u32 = 0x07;

/// The unit of debug information of a module, which every function of it that is marked
/// ([`Defined::marked`]) is a subprogram of. `None` where no line table is read back.
pub(super) struct Debug<'ctx> {
    builder: DebugInfoBuilder<'ctx>,
    unit: DICompileUnit<'ctx>,
}

impl<'ctx> Debug<'ctx> {
    pub(super) fn of(module: &Module<'ctx>) -> Lowered<Option<Debug<'ctx>>> {
        // Without a flag that asks for CodeView, the table is DWARF in a COFF object too.
        let target = crate::Target::selected().map_err(Unhandled::Llvm)?;
        if !matches!(target.object_format, ObjectFormat::Elf | ObjectFormat::Coff) {
            return Ok(None);
        }
        let context = module.get_context();
        let version = context.i32_type().const_int(debug_metadata_version().into(), false);
        module.add_basic_value_flag("Debug Info Version", FlagBehavior::Warning, version);
        // The places are written into a unit of this version (see `debuginfo`).
        let dwarf = context.i32_type().const_int(4, false);
        module.add_basic_value_flag("Dwarf Version", FlagBehavior::Warning, dwarf);
        let (builder, unit) = module.create_debug_info_builder(
            false,
            DWARFSourceLanguage::Rust,
            "blaze",
            "",
            "blaze",
            false,
            "",
            0,
            "",
            DWARFEmissionKind::Full,
            0,
            false,
            false,
            "",
            "",
        );
        Ok(Some(Debug { builder, unit }))
    }

    /// Ends the unit, once every function of the module is lowered.
    pub(super) fn finish(&self) {
        self.builder.finalize();
    }
}

struct Marks<'ctx, 'f> {
    debug: &'f Debug<'ctx>,
    function: DISubprogram<'ctx>,
    /// The file of the function's lines, named as the function is: the lines of a function
    /// LLVM built into another of the module are of the other's, and are told apart by it.
    file: DIFile<'ctx>,
}

impl<'ctx, 'f> Marks<'ctx, 'f> {
    fn new(debug: &'f Debug<'ctx>, function: FunctionValue<'ctx>) -> Marks<'ctx, 'f> {
        let Debug { builder, unit } = debug;
        let name = function.get_name().to_string_lossy();
        let file = builder.create_file(&name, "");
        let ty = builder.create_subroutine_type(file, None, &[], DIFlags::ZERO);
        let subprogram = builder.create_function(
            unit.as_debug_info_scope(),
            &name,
            None,
            file,
            0,
            ty,
            false,
            true,
            0,
            DIFlags::ZERO,
            false,
        );
        function.set_subprogram(subprogram);
        Marks { debug, function: subprogram, file }
    }
}

impl<'ctx, 'f> Lowering<'ctx, 'f> {
    /// The parameters of `func` that are addresses: those whose types say so, and those
    /// used as one.
    fn pointers(
        func: &ir::Function,
        pointees: &[(u32, Pointee)],
        addresses: &FxHashSet<Value>,
    ) -> Vec<u32> {
        let mut pointers: Vec<u32> = pointees.iter().map(|it| it.0).collect();
        if let Some(entry) = func.layout.entry_block() {
            let params = func.dfg.block_params(entry).iter().zip(&func.signature.params);
            for (at, (param, abi)) in params.enumerate() {
                if abi.purpose == ArgumentPurpose::Normal
                    && addresses.contains(param)
                    && !pointers.contains(&(at as u32))
                {
                    pointers.push(at as u32);
                }
            }
        }
        pointers
    }

    /// Adds the function `defined` is to `module`, with the attributes of its signature.
    pub(super) fn add(
        context: &'ctx Context,
        module: &Module<'ctx>,
        defined: &Defined<'_>,
    ) -> Lowered<FunctionValue<'ctx>> {
        let func = defined.func;
        let addresses = addresses(func, &Flow::of(func));
        let pointers = Self::pointers(func, defined.pointees, &addresses);
        let ty = Self::function_type(context, &func.signature)?;
        let ty = Self::with_pointers(context, ty, &pointers);
        let function = module.add_function(defined.name, ty, Some(Linkage::External));
        Self::mark_pointees(context, defined.pointees, |loc, attribute| {
            function.add_attribute(loc, attribute)
        });
        if let Some(weights) = defined.weights {
            entry_count(context, function, weights.entered);
        }
        Self::mark_signature(context, &func.signature, Side::Callee, |loc, attribute| {
            function.add_attribute(loc, attribute)
        });
        let target = crate::Target::selected().map_err(Unhandled::Llvm)?;
        let probe = target.stack_probe().map(|probe| ("probe-stack", probe));
        // Without, LLVM gives the frame pointer's register to values, and code that walks the
        // frames by it reads one of those.
        let frame = crate::backend::frame_pointers().then_some(("frame-pointer", "all"));
        for (name, value) in [("target-cpu", target.cpu())].into_iter().chain(probe).chain(frame) {
            let attribute = context.create_string_attribute(name, value);
            function.add_attribute(AttributeLoc::Function, attribute);
        }
        // Every function gets an unwind table entry, as a panic may cross any of them.
        let uwtable = super::attribute_kind("uwtable");
        function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(uwtable, 2));
        Ok(function)
    }

    /// Gives `function`, which [`Lowering::add`] added for `defined`, its body. A call
    /// of a function `shown` has the body of goes to that body.
    pub(super) fn define(
        context: &'ctx Context,
        module: &'f Module<'ctx>,
        function: FunctionValue<'ctx>,
        defined: &Defined<'f>,
        constants: &'f FxHashMap<u32, &'f [u8]>,
        statics: &'f FxHashSet<u32>,
        resume: Option<u32>,
        memcmp: Option<u32>,
        blocks_asm: &'f FxHashMap<u32, Arc<asm::Inline>>,
        shown: &'f FxHashMap<u32, FunctionValue<'ctx>>,
        debug: Option<&'f Debug<'ctx>>,
    ) -> Lowered<()> {
        let func = defined.func;
        let flow = Flow::of(func);
        let addresses = addresses(func, &flow);
        let pointers = Self::pointers(func, defined.pointees, &addresses);
        let pointer_values: Vec<Value> = func.layout.entry_block().map_or_else(Vec::new, |entry| {
            let params = func.dfg.block_params(entry);
            pointers.iter().filter_map(|&at| params.get(at as usize).copied()).collect()
        });
        let pointerish = pointerish(func, &flow, &addresses, &pointer_values);
        let mut lowering = Lowering {
            context,
            module,
            builder: context.create_builder(),
            func,
            function,
            values: FxHashMap::default(),
            conditions: FxHashMap::default(),
            blocks: FxHashMap::default(),
            phis: FxHashMap::default(),
            slots: FxHashMap::default(),
            callees: FxHashMap::default(),
            globals: FxHashMap::default(),
            constants,
            statics,
            resume,
            memcmp,
            blocks_asm,
            pads: FxHashMap::default(),
            frame: None,
            arrival: None,
            marks: debug.filter(|_| defined.marked).map(|debug| Marks::new(debug, function)),
            weights: defined.weights,
            addresses,
            pointer_params: FxHashMap::default(),
            pointerish,
            realigned: realigned(func),
            shown,
        };
        lowering.pointer_params = lowering.pointer_params(&lowering.pointerish);
        lowering.body(defined.named)
    }

    /// Has what is built next be of the source location `inst` has.
    fn mark(&self, inst: Inst) {
        let Some(marks) = &self.marks else { return };
        let scope = marks.function.as_debug_info_scope();
        let line = unwind::seh::unguarded(self.func.srcloc(inst)).bits();
        let location =
            marks.debug.builder.create_debug_location(self.context, line, 0, scope, None);
        self.builder.set_current_debug_location(location);
    }

    /// Tells LLVM how often `branch` went to its first block and to its second, as
    /// `branch_weights`, which are 32 bits: counts past that are scaled down together.
    fn weigh(&self, branch: InstructionValue<'ctx>, [then, otherwise]: [u64; 2]) -> Lowered<()> {
        let scale = (then.max(otherwise) >> 31) + 1;
        let i32 = self.context.i32_type();
        let node = self.context.metadata_node(&[
            self.context.metadata_string("branch_weights").into(),
            i32.const_int(then / scale, false).into(),
            i32.const_int(otherwise / scale, false).into(),
        ]);
        branch
            .set_metadata(node, self.context.get_kind_id("prof"))
            .map_err(|e| Unhandled::Llvm(e.to_string()))
    }

    /// Tells LLVM that `alloca` is a variable of `size` bytes named by the number of `slot`,
    /// so that it says where the bytes of the slot are kept once it is done with them.
    fn declare(
        &self,
        slot: ir::StackSlot,
        alloca: PointerValue<'ctx>,
        size: u32,
        block: BasicBlock<'ctx>,
    ) -> Lowered<()> {
        let Some(marks) = &self.marks else { return Ok(()) };
        let name = slot.as_u32().to_string();
        let ty = marks
            .debug
            .builder
            .create_basic_type(&name, u64::from(size) * 8, DW_ATE_UNSIGNED, DIFlags::ZERO)
            .map_err(|e| Unhandled::Llvm(e.to_string()))?;
        let scope = marks.function.as_debug_info_scope();
        let variable = marks.debug.builder.create_auto_variable(
            scope,
            &name,
            marks.file,
            0,
            ty.as_type(),
            true,
            DIFlags::ZERO,
            0,
        );
        let location = marks.debug.builder.create_debug_location(self.context, 0, 0, scope, None);
        marks.debug.builder.insert_declare_at_end(alloca, Some(variable), None, location, block);
        Ok(())
    }

    /// 1. Allocas for the stack slots, in a block ahead of the entry block.
    /// 2. A block and its phis for every reachable block.
    /// 3. The instructions, in an order that has every value defined before it is used.
    fn body(&mut self, named: &[ir::StackSlot]) -> Lowered<()> {
        let Some(entry) = self.func.layout.entry_block() else {
            return Err(Unhandled::Shape("function without blocks"));
        };
        let frame = self.context.append_basic_block(self.function, "frame");
        self.frame = Some(frame);
        self.builder.position_at_end(frame);
        for (slot, data) in self.func.sized_stack_slots.iter() {
            let ty = self.context.i8_type().array_type(data.size);
            let alloca = self.builder.build_alloca(ty, "")?;
            let rounded = self.realigned.values().filter(|it| it.0 == slot).map(|it| it.1);
            let align = rounded.fold(1u32 << data.align_shift, u32::max);
            alloca
                .as_instruction()
                .expect("an alloca")
                .set_alignment(align)
                .map_err(|e| Unhandled::Llvm(e.to_string()))?;
            self.slots.insert(slot, alloca);
            if named.contains(&slot) {
                self.declare(slot, alloca, data.size, frame)?;
            }
        }

        let order = self.reverse_post_order(entry);
        for &block in &order {
            let lowered = self.context.append_basic_block(self.function, "");
            self.blocks.insert(block, lowered);
            if block == entry {
                continue;
            }
            self.builder.position_at_end(lowered);
            let mut phis = Vec::new();
            for &param in self.func.dfg.block_params(block) {
                let ty = match self.pointerish.contains(&param) {
                    true => self.context.ptr_type(AddressSpace::default()).into(),
                    false => self.ty(self.func.dfg.value_type(param))?,
                };
                let phi = self.builder.build_phi(ty, "")?;
                self.values.insert(param, phi.as_basic_value().into());
                phis.push(phi);
            }
            self.phis.insert(block, phis);
        }
        self.builder.position_at_end(frame);
        for (at, &param) in self.func.dfg.block_params(entry).iter().enumerate() {
            let value = match self.func.signature.params[at].purpose {
                ArgumentPurpose::StructArgument(_)
                    if self.func.signature.call_conv == CallConv::WindowsFastcall =>
                {
                    Operand::Pointer(self.stack_argument(at)?)
                }
                _ => self.function.get_nth_param(at as u32).expect("a parameter").into(),
            };
            self.values.insert(param, value);
        }
        self.builder.build_unconditional_branch(self.blocks[&entry])?;

        for block in order {
            self.builder.position_at_end(self.blocks[&block]);
            for inst in self.func.layout.block_insts(block) {
                self.mark(inst);
                self.inst(inst)?;
            }
        }
        Ok(())
    }

    fn reverse_post_order(&self, entry: Block) -> Vec<Block> {
        let mut order = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        seen.insert(entry);
        let mut stack = vec![(entry, self.successors(entry), 0)];
        while let Some((block, successors, next)) = stack.last_mut() {
            match successors.get(*next) {
                Some(&successor) => {
                    *next += 1;
                    if seen.insert(successor) {
                        stack.push((successor, self.successors(successor), 0));
                    }
                }
                None => {
                    order.push(*block);
                    stack.pop();
                }
            }
        }
        order.reverse();
        order
    }

    fn successors(&self, block: Block) -> Vec<Block> {
        let dfg = &self.func.dfg;
        let Some(last) = self.func.layout.last_inst(block) else { return Vec::new() };
        dfg.insts[last]
            .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
            .iter()
            .map(|call| call.block(&dfg.value_lists))
            .collect()
    }

    fn ty(&self, ty: Type) -> Lowered<BasicTypeEnum<'ctx>> {
        Self::basic_type(self.context, ty)
    }

    fn basic_type(context: &'ctx Context, ty: Type) -> Lowered<BasicTypeEnum<'ctx>> {
        if ty.is_vector() && !ty.is_dynamic_vector() {
            let lanes = ty.lane_count();
            return Ok(match Self::basic_type(context, ty.lane_type())? {
                BasicTypeEnum::IntType(lane) => lane.vec_type(lanes).into(),
                BasicTypeEnum::FloatType(lane) => lane.vec_type(lanes).into(),
                _ => return Err(Unhandled::Type(ty)),
            });
        }
        Ok(match ty {
            types::F16 => context.f16_type().into(),
            types::F32 => context.f32_type().into(),
            types::F64 => context.f64_type().into(),
            types::F128 => context.f128_type().into(),
            _ if ty.is_int() => Self::int_of_width(context, ty.bits())?.into(),
            _ => return Err(Unhandled::Type(ty)),
        })
    }

    fn int_of_width(context: &'ctx Context, bits: u32) -> Lowered<IntType<'ctx>> {
        NonZeroU32::new(bits)
            .and_then(|bits| context.custom_width_int_type(bits).ok())
            .ok_or(Unhandled::Shape("an integer of no width"))
    }

    fn int_ty(&self, ty: Type) -> Lowered<IntType<'ctx>> {
        match self.ty(ty)? {
            BasicTypeEnum::IntType(it) => Ok(it),
            _ => Err(Unhandled::Type(ty)),
        }
    }

    /// A parameter that stands for memory is a pointer to LLVM; the rest are what they are.
    fn function_type(context: &'ctx Context, sig: &Signature) -> Lowered<FunctionType<'ctx>> {
        // LLVM would give the register such an integer leaves to the next parameter, which
        // Cranelift, building the other side of the call, does not. The Rust and C ABIs
        // pass it in memory instead; what is left is a signature with a parameter put in
        // front of them, as a vtable's receiver.
        if sig.call_conv == CallConv::SystemV
            && crate::Target::current().arch == crate::target::Arch::X86_64
            && crate::lone_register_i128(&sig.params).is_some()
        {
            return Err(Unhandled::Shape("a 128-bit integer that finds one register left"));
        }
        // On AArch64 LLVM takes the integer registers such an integer leaves out of use and
        // Cranelift does not; the Rust ABI passes nothing in them.
        let even = match sig.call_conv {
            CallConv::AppleAarch64 => 1,
            _ => 2,
        };
        if crate::Target::current().arch == crate::target::Arch::Aarch64
            && crate::stranded_i128(&sig.params, even).is_some()
        {
            return Err(Unhandled::Shape(
                "a 128-bit integer that goes to the stack beside free registers",
            ));
        }
        let pointer = context.ptr_type(AddressSpace::default());
        let mut params: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::with_capacity(sig.params.len());
        for param in &sig.params {
            params.push(match param.purpose {
                ArgumentPurpose::Normal => Self::basic_type(context, param.value_type)?.into(),
                ArgumentPurpose::StructReturn | ArgumentPurpose::StructArgument(_) => {
                    pointer.into()
                }
                _ => return Err(Unhandled::Shape("special-purpose parameter")),
            });
        }
        Ok(match sig.returns.as_slice() {
            [] => context.void_type().fn_type(&params, false),
            [one] => Self::basic_type(context, one.value_type)?.fn_type(&params, false),
            // Two scalars come back in two registers, as an LLVM struct of two does.
            [first, second] => {
                let fields = [
                    Self::basic_type(context, first.value_type)?,
                    Self::basic_type(context, second.value_type)?,
                ];
                context.struct_type(&fields, false).fn_type(&params, false)
            }
            // AArch64 has eight registers of each kind for what comes back, and LLVM gives
            // the fields of a struct the next of its kind in order, as Cranelift does values.
            many if crate::Target::current().arch == crate::target::Arch::Aarch64
                && many.len() <= 8 =>
            {
                let fields = many
                    .iter()
                    .map(|it| Self::basic_type(context, it.value_type))
                    .collect::<Lowered<Vec<_>>>()?;
                context.struct_type(&fields, false).fn_type(&params, false)
            }
            _ => return Err(Unhandled::Shape("more than two return values")),
        })
    }

    /// `ty` with the parameters at `pointers` taken as pointers, which they are passed as
    /// whatever their type: an address is in an integer register either way.
    fn with_pointers(
        context: &'ctx Context,
        ty: FunctionType<'ctx>,
        pointers: &[u32],
    ) -> FunctionType<'ctx> {
        if pointers.is_empty() {
            return ty;
        }
        let pointer = context.ptr_type(AddressSpace::default());
        let mut params: Vec<BasicMetadataTypeEnum<'ctx>> =
            ty.get_param_types().into_iter().map(Into::into).collect();
        for &at in pointers {
            if let Some(param) = params.get_mut(at as usize) {
                *param = pointer.into();
            }
        }
        match ty.get_return_type() {
            Some(returned) => returned.fn_type(&params, false),
            None => context.void_type().fn_type(&params, false),
        }
    }

    /// Whether `a` is `b` but for parameters that are pointers in one and integers of their
    /// width in the other, which are passed alike.
    pub(super) fn same_but_pointers(a: FunctionType<'ctx>, b: FunctionType<'ctx>) -> bool {
        let (left, right) = (a.get_param_types(), b.get_param_types());
        let address = |ty: &BasicMetadataTypeEnum<'ctx>| match ty {
            BasicMetadataTypeEnum::PointerType(_) => true,
            BasicMetadataTypeEnum::IntType(int) => int.get_bit_width() == 64,
            _ => false,
        };
        a.get_return_type() == b.get_return_type()
            && left.len() == right.len()
            && std::iter::zip(&left, &right).all(|(l, r)| l == r || (address(l) && address(r)))
    }

    /// What the types of the parameters at the places of `pointees` say of their memory.
    fn mark_pointees(
        context: &'ctx Context,
        pointees: &[(u32, Pointee)],
        mut add: impl FnMut(AttributeLoc, Attribute),
    ) {
        let attribute =
            |name, value| context.create_enum_attribute(super::attribute_kind(name), value);
        for &(at, pointee) in pointees {
            let loc = AttributeLoc::Param(at);
            add(loc, attribute("noundef", 0));
            add(loc, attribute("nonnull", 0));
            add(loc, attribute("align", pointee.align.max(1)));
            if pointee.size != 0 {
                add(loc, attribute("dereferenceable", pointee.size));
            }
            if pointee.unique {
                add(loc, attribute("noalias", 0));
            }
            if pointee.frozen {
                add(loc, attribute("readonly", 0));
            }
        }
    }

    /// The attributes of a signature, for the function being compiled or for a call.
    ///
    /// A small integer is extended by whoever hands it over and by nobody else: rustc does
    /// not extend what it passes by the Rust ABI, so the code of a function may not count on
    /// it for its parameters, nor the code around a call for what the call returns.
    fn mark_signature(
        context: &'ctx Context,
        sig: &Signature,
        side: Side,
        mut add: impl FnMut(AttributeLoc, Attribute),
    ) {
        let kind = super::attribute_kind;
        let extension = |extension| match extension {
            ArgumentExtension::None => None,
            ArgumentExtension::Uext => Some(context.create_enum_attribute(kind("zeroext"), 0)),
            ArgumentExtension::Sext => Some(context.create_enum_attribute(kind("signext"), 0)),
        };
        let i8 = context.i8_type();
        for (at, param) in sig.params.iter().enumerate() {
            let loc = AttributeLoc::Param(at as u32);
            match param.purpose {
                ArgumentPurpose::StructReturn => {
                    add(loc, context.create_type_attribute(kind("sret"), i8.into()));
                    // Where a value is returned to is not kept, as rustc tells LLVM:
                    // `captures(none)`, whose value 0 is LLVM's `CaptureInfo::none()`.
                    add(loc, context.create_enum_attribute(kind("captures"), 0));
                }
                ArgumentPurpose::StructArgument(size) => {
                    let ty = i8.array_type(size);
                    add(loc, context.create_type_attribute(kind("byval"), ty.into()));
                    add(loc, context.create_enum_attribute(kind("align"), 8));
                }
                _ => {}
            }
            if let (Side::Caller, Some(attribute)) = (side, extension(param.extension)) {
                add(loc, attribute);
            }
        }
        if let (Side::Callee, [one]) = (side, sig.returns.as_slice())
            && let Some(attribute) = extension(one.extension)
        {
            add(AttributeLoc::Return, attribute);
        }
    }

    fn operand(&self, value: Value) -> Operand<'ctx> {
        self.values[&self.func.dfg.resolve_aliases(value)]
    }

    fn set(&mut self, inst: Inst, value: impl Into<Operand<'ctx>>) {
        self.values.insert(self.func.dfg.first_result(inst), value.into());
    }

    fn int(&self, value: Value) -> Lowered<IntValue<'ctx>> {
        match self.operand(value) {
            Operand::Int(it) => Ok(it),
            Operand::Pointer(it) => {
                Ok(self.builder.build_ptr_to_int(it, self.context.i64_type(), "")?)
            }
            _ => Err(Unhandled::Type(self.func.dfg.value_type(value))),
        }
    }

    fn pointer(&self, value: Value) -> Lowered<PointerValue<'ctx>> {
        match self.operand(value) {
            Operand::Pointer(it) => Ok(it),
            Operand::Int(it) => Ok(self.builder.build_int_to_ptr(
                it,
                self.context.ptr_type(AddressSpace::default()),
                "",
            )?),
            _ => Err(Unhandled::Type(self.func.dfg.value_type(value))),
        }
    }

    fn float(&self, value: Value) -> Lowered<FloatValue<'ctx>> {
        match self.operand(value) {
            Operand::Float(it) => Ok(it),
            _ => Err(Unhandled::Type(self.func.dfg.value_type(value))),
        }
    }

    fn basic(&self, value: Value) -> BasicValueEnum<'ctx> {
        match self.operand(value) {
            Operand::Int(it) => it.into(),
            Operand::Pointer(it) => it.into(),
            Operand::Float(it) => it.into(),
            Operand::Vector(it) => it.into(),
        }
    }

    /// A value as the type Cranelift gave it: a pointer goes back to being an integer.
    fn plain(&self, value: Value) -> Lowered<BasicValueEnum<'ctx>> {
        match self.operand(value) {
            Operand::Pointer(_) => Ok(self.int(value)?.into()),
            _ => Ok(self.basic(value)),
        }
    }

    /// `value` as `phi` takes it: a pointer for a phi of pointers.
    fn passed(&self, phi: PhiValue<'ctx>, value: Value) -> Lowered<BasicValueEnum<'ctx>> {
        match phi.as_basic_value() {
            BasicValueEnum::PointerValue(_) => Ok(self.pointer(value)?.into()),
            _ => self.plain(value),
        }
    }

    /// The same for a value of LLVM's.
    fn as_phi(
        &self,
        phi: PhiValue<'ctx>,
        value: BasicValueEnum<'ctx>,
    ) -> Lowered<BasicValueEnum<'ctx>> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        Ok(match (phi.as_basic_value(), value) {
            (BasicValueEnum::PointerValue(_), BasicValueEnum::IntValue(int)) => {
                self.builder.build_int_to_ptr(int, pointer, "")?.into()
            }
            (BasicValueEnum::IntValue(want), BasicValueEnum::PointerValue(address)) => {
                self.builder.build_ptr_to_int(address, want.get_type(), "")?.into()
            }
            _ => value,
        })
    }

    fn condition(&self, value: Value) -> Lowered<IntValue<'ctx>> {
        let value = self.func.dfg.resolve_aliases(value);
        if let Some(&condition) = self.conditions.get(&value) {
            return Ok(condition);
        }
        let int = self.int(value)?;
        let zero = int.get_type().const_zero();
        Ok(self.builder.build_int_compare(IntPredicate::NE, int, zero, "")?)
    }

    /// Records a comparison, both as the `i1` it is and as the byte Cranelift has it as.
    fn set_condition(&mut self, inst: Inst, condition: IntValue<'ctx>) -> Lowered<()> {
        let result = self.func.dfg.first_result(inst);
        let byte = self.builder.build_int_z_extend(condition, self.context.i8_type(), "")?;
        self.conditions.insert(result, condition);
        self.values.insert(result, Operand::Int(byte));
        Ok(())
    }

    fn offset(&self, base: PointerValue<'ctx>, by: IntValue<'ctx>) -> Lowered<PointerValue<'ctx>> {
        // SAFETY: not `inbounds`, so any offset is defined.
        Ok(unsafe { self.builder.build_gep(self.context.i8_type(), base, &[by], "")? })
    }

    fn address(&self, base: Value, offset: i32) -> Lowered<PointerValue<'ctx>> {
        let base = self.pointer(base)?;
        if offset == 0 {
            return Ok(base);
        }
        self.offset(base, self.context.i64_type().const_int(offset as i64 as u64, true))
    }

    /// An immediate as a constant of `ty`. Cranelift keeps 64 bits of it, sign-extended.
    fn immediate(&self, ty: IntType<'ctx>, imm: i64, signed: bool) -> IntValue<'ctx> {
        if ty.get_bit_width() <= 64 {
            return ty.const_int(imm as u64, false);
        }
        let high = if signed && imm < 0 { u64::MAX } else { 0 };
        ty.const_int_arbitrary_precision(&[imm as u64, high])
    }

    fn intrinsic(
        &self,
        name: &str,
        types: &[BasicTypeEnum<'ctx>],
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Lowered<BasicValueEnum<'ctx>> {
        let function = Intrinsic::find(name)
            .and_then(|it| it.get_declaration(self.module, types))
            .ok_or_else(|| Unhandled::Llvm(format!("no intrinsic `{name}`")))?;
        let call = self.builder.build_call(function, args, "")?;
        call.try_as_basic_value()
            .basic()
            .ok_or_else(|| Unhandled::Llvm(format!("`{name}` returns nothing")))
    }

    fn int_intrinsic(&self, name: &str, args: &[IntValue<'ctx>]) -> Lowered<IntValue<'ctx>> {
        let ty = args[0].get_type();
        let args: Vec<_> = args.iter().map(|&it| it.into()).collect();
        Ok(self.intrinsic(name, &[ty.into()], &args)?.into_int_value())
    }

    fn float_intrinsic(&self, name: &str, args: &[FloatValue<'ctx>]) -> Lowered<FloatValue<'ctx>> {
        let ty = args[0].get_type();
        let args: Vec<_> = args.iter().map(|&it| it.into()).collect();
        Ok(self.intrinsic(name, &[ty.into()], &args)?.into_float_value())
    }

    fn inst(&mut self, inst: Inst) -> Lowered<()> {
        let dfg = &self.func.dfg;
        let data = dfg.insts[inst];
        let opcode = data.opcode();
        match data {
            InstructionData::UnaryImm { imm, .. } => {
                let ty = self.int_ty(dfg.ctrl_typevar(inst))?;
                self.set(inst, Operand::Int(self.immediate(ty, imm.bits(), true)));
            }
            InstructionData::UnaryIeee32 { imm, .. } => {
                let bits = self.context.i32_type().const_int(imm.bits().into(), false);
                let float = self.builder.build_bit_cast(bits, self.context.f32_type(), "")?;
                self.set(inst, float);
            }
            InstructionData::UnaryIeee64 { imm, .. } => {
                let bits = self.context.i64_type().const_int(imm.bits(), false);
                let float = self.builder.build_bit_cast(bits, self.context.f64_type(), "")?;
                self.set(inst, float);
            }
            InstructionData::Unary { arg, .. } => self.unary(inst, opcode, arg)?,
            InstructionData::Binary { args: [l, r], .. } => self.binary(inst, opcode, l, r)?,
            InstructionData::IntCompare { args: [l, r], cond, .. } => {
                let predicate = Self::int_predicate(cond);
                if let (Operand::Vector(l), Operand::Vector(r)) = (self.operand(l), self.operand(r))
                {
                    // A lane that holds is all ones, as in Cranelift.
                    let holds = self.builder.build_int_compare(predicate, l, r, "")?;
                    let mask = self.builder.build_int_s_extend(holds, l.get_type(), "")?;
                    self.set(inst, Operand::Vector(mask));
                    return Ok(());
                }
                // Two addresses are compared as pointers: as integers they would have
                // escaped.
                let compared = match (self.operand(l), self.operand(r)) {
                    (Operand::Pointer(_), _) | (_, Operand::Pointer(_)) => {
                        let (l, r) = (self.pointer(l)?, self.pointer(r)?);
                        self.builder.build_int_compare(predicate, l, r, "")?
                    }
                    _ => {
                        self.builder.build_int_compare(predicate, self.int(l)?, self.int(r)?, "")?
                    }
                };
                self.set_condition(inst, compared)?;
            }
            InstructionData::FloatCompare { args: [l, r], cond, .. } => {
                let (l, r) = (self.float(l)?, self.float(r)?);
                let predicate = Self::float_predicate(cond);
                let compared = self.builder.build_float_compare(predicate, l, r, "")?;
                self.set_condition(inst, compared)?;
            }
            InstructionData::Ternary { args: [a, b, c], .. } => match opcode {
                Opcode::Select => {
                    let condition = self.condition(a)?;
                    let address = self.addresses.contains(&dfg.first_result(inst));
                    let (then, otherwise) = match (self.operand(b), self.operand(c)) {
                        (Operand::Pointer(_), Operand::Pointer(_)) => {
                            (self.basic(b), self.basic(c))
                        }
                        _ if address => (self.pointer(b)?.into(), self.pointer(c)?.into()),
                        _ => (self.plain(b)?, self.plain(c)?),
                    };
                    let selected = self.builder.build_select(condition, then, otherwise, "")?;
                    self.set(inst, selected);
                }
                Opcode::Fma if self.func.dfg.value_type(a).is_vector() => {
                    // Cranelift's AArch64 vector fma is `fmla c, a, b`, LLVM's of the same
                    // operands `fmla c, b, a`: which factor comes first decides which of two
                    // NaNs comes out.
                    let factors = match crate::Target::current().arch {
                        crate::target::Arch::Aarch64 => [b, a],
                        _ => [a, b],
                    };
                    let ty = self.ty(self.func.dfg.value_type(a))?;
                    let args = [factors[0], factors[1], c].map(|it| self.basic(it).into());
                    self.set(inst, self.intrinsic("llvm.fma", &[ty], &args)?);
                }
                Opcode::Fma => {
                    let args = [self.float(a)?, self.float(b)?, self.float(c)?];
                    let fused = self.float_intrinsic("llvm.fma", &args)?;
                    self.set(inst, Operand::Float(fused));
                }
                Opcode::Bitselect => {
                    let (mask, then, otherwise) = (self.int(a)?, self.int(b)?, self.int(c)?);
                    let then = self.builder.build_and(then, mask, "")?;
                    let unmasked = self.builder.build_not(mask, "")?;
                    let otherwise = self.builder.build_and(otherwise, unmasked, "")?;
                    self.set(inst, Operand::Int(self.builder.build_or(then, otherwise, "")?));
                }
                _ => return Err(Unhandled::Opcode(opcode)),
            },
            InstructionData::Load { arg, offset, flags, .. } => {
                self.load(inst, opcode, arg, offset.into(), dfg.mem_flags[flags].aligned())?;
            }
            InstructionData::Store { args: [value, address], offset, flags, .. } => {
                let address = self.address(address, offset.into())?;
                self.store(opcode, value, address, dfg.mem_flags[flags].aligned())?;
            }
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                let slot = self.slots[&stack_slot];
                let offset: i32 = offset.into();
                let address = match offset {
                    0 => slot,
                    _ => {
                        self.offset(slot, self.context.i64_type().const_int(offset as u64, true))?
                    }
                };
                self.set(inst, Operand::Pointer(address));
            }
            InstructionData::UnaryGlobalValue { global_value, .. } => {
                let global = self.global(global_value)?;
                self.set(inst, Operand::Pointer(global));
            }
            InstructionData::FuncAddr { func_ref, .. } => {
                let callee = self.callee(func_ref)?;
                self.set(inst, Operand::Pointer(callee.as_global_value().as_pointer_value()));
            }
            InstructionData::Call { func_ref, .. } => {
                if let Some(libcall) = memory_call(&dfg.ext_funcs[func_ref].name) {
                    let to = self.memory(libcall, dfg.inst_args(inst))?;
                    if !dfg.inst_results(inst).is_empty() {
                        self.set(inst, Operand::Pointer(to));
                    }
                    return Ok(());
                }
                if let [exception] = *dfg.inst_args(inst)
                    && self.resumes(func_ref)
                {
                    return self.resume(exception);
                }
                if let [slot] = *dfg.inst_args(inst)
                    && dfg.inst_results(inst).is_empty()
                    && let Some(block) = self.asm_block(func_ref)
                {
                    return self.inline_asm(&block, slot);
                }
                let callee = self.callee(func_ref)?;
                let sig = &dfg.signatures[dfg.ext_funcs[func_ref].signature];
                let ty = callee.get_type();
                if unwind::seh::is_guarded(self.func.srcloc(inst)) {
                    let (returned, pad) = self.guard();
                    let args = self.invoke_arguments(ty, sig, dfg.inst_args(inst))?;
                    let call =
                        self.builder.build_direct_invoke(callee, &args, returned, pad, "")?;
                    return self.finish_guarded(call, sig, dfg.inst_results(inst), returned, pad);
                }
                let args = self.arguments(ty, sig, dfg.inst_args(inst))?;
                let call = self.builder.build_direct_call(callee, &args, "")?;
                self.finish_call(call, sig, dfg.inst_results(inst))?;
            }
            InstructionData::CallIndirect { sig_ref, .. } => {
                let sig = &dfg.signatures[sig_ref];
                let [callee, args @ ..] = dfg.inst_args(inst) else {
                    return Err(Unhandled::Shape("indirect call without a callee"));
                };
                let ty = self.indirect_type(sig, args)?;
                let callee = self.pointer(*callee)?;
                if unwind::seh::is_guarded(self.func.srcloc(inst)) {
                    let (returned, pad) = self.guard();
                    let args = self.invoke_arguments(ty, sig, args)?;
                    let call =
                        self.builder.build_indirect_invoke(ty, callee, &args, returned, pad, "")?;
                    return self.finish_guarded(call, sig, dfg.inst_results(inst), returned, pad);
                }
                let args = self.arguments(ty, sig, args)?;
                let call = self.builder.build_indirect_call(ty, callee, &args, "")?;
                self.finish_call(call, sig, dfg.inst_results(inst))?;
            }
            InstructionData::TryCall { func_ref, exception, .. } => {
                if let Some(libcall) = memory_call(&dfg.ext_funcs[func_ref].name) {
                    let to = self.memory(libcall, dfg.inst_args(inst))?;
                    let from = self.builder.get_insert_block().expect("a block being filled");
                    return self.returned_to(exception, &[to.into()], from);
                }
                if let [unwinding] = *dfg.inst_args(inst)
                    && self.resumes(func_ref)
                {
                    return self.unwound_to(exception, unwinding);
                }
                let callee = self.callee(func_ref)?;
                let (normal, pad) = self.exception_edges(exception)?;
                let sig = &dfg.signatures[dfg.ext_funcs[func_ref].signature];
                let args = self.invoke_arguments(callee.get_type(), sig, dfg.inst_args(inst))?;
                let call = self.builder.build_direct_invoke(callee, &args, normal, pad, "")?;
                self.finish_invoke(call, sig, exception, normal)?;
            }
            InstructionData::TryCallIndirect { exception, .. } => {
                let sig = &dfg.signatures[dfg.exception_tables[exception].signature()];
                let [callee, args @ ..] = dfg.inst_args(inst) else {
                    return Err(Unhandled::Shape("indirect call without a callee"));
                };
                let ty = self.indirect_type(sig, args)?;
                let callee = self.pointer(*callee)?;
                let (normal, pad) = self.exception_edges(exception)?;
                let args = self.invoke_arguments(ty, sig, args)?;
                let call =
                    self.builder.build_indirect_invoke(ty, callee, &args, normal, pad, "")?;
                self.finish_invoke(call, sig, exception, normal)?;
            }
            InstructionData::Jump { destination, .. } => {
                let target = self.edge(destination)?;
                self.builder.build_unconditional_branch(target)?;
            }
            InstructionData::Brif { arg, blocks: [then, otherwise], .. } => {
                let condition = self.condition(arg)?;
                let (then, otherwise) = (self.branch_edge(then)?, self.branch_edge(otherwise)?);
                let branch = self.builder.build_conditional_branch(condition, then, otherwise)?;
                if let Some(taken) = self.weights.and_then(|it| it.taken.get(&inst)) {
                    self.weigh(branch, *taken)?;
                }
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let index = self.int(arg)?;
                let table = &dfg.jump_tables[table];
                let default = self.branch_edge(table.default_block())?;
                let mut cases = Vec::with_capacity(table.as_slice().len());
                for (at, &call) in table.as_slice().iter().enumerate() {
                    let case = index.get_type().const_int(at as u64, false);
                    cases.push((case, self.branch_edge(call)?));
                }
                self.builder.build_switch(index, default, &cases)?;
            }
            InstructionData::MultiAry { opcode: Opcode::Return, .. } => {
                match dfg.inst_args(inst) {
                    [] => self.builder.build_return(None)?,
                    [one] => self.builder.build_return(Some(&self.plain(*one)?))?,
                    many => {
                        let values =
                            many.iter().map(|&it| self.plain(it)).collect::<Lowered<Vec<_>>>()?;
                        self.builder.build_aggregate_return(&values)?
                    }
                };
            }
            InstructionData::Trap { .. } => self.trap()?,
            InstructionData::CondTrap { arg, .. } => {
                let condition = self.condition(arg)?;
                let trap = self.context.append_basic_block(self.function, "");
                let resume = self.context.append_basic_block(self.function, "");
                match opcode {
                    Opcode::Trapnz => {
                        self.builder.build_conditional_branch(condition, trap, resume)?
                    }
                    Opcode::Trapz => {
                        self.builder.build_conditional_branch(condition, resume, trap)?
                    }
                    _ => return Err(Unhandled::Opcode(opcode)),
                };
                self.builder.position_at_end(trap);
                self.trap()?;
                self.builder.position_at_end(resume);
            }
            InstructionData::LoadNoOffset { arg, .. } => match opcode {
                Opcode::AtomicLoad => {
                    let address = self.pointer(arg)?;
                    let ty = self.ty(dfg.ctrl_typevar(inst))?;
                    let loaded = self.builder.build_load(ty, address, "")?;
                    Self::make_atomic(loaded.as_instruction_value().expect("a load"))?;
                    self.set(inst, loaded);
                }
                Opcode::Bitcast => self.bitcast(inst, arg)?,
                _ => return Err(Unhandled::Opcode(opcode)),
            },
            InstructionData::StoreNoOffset { args: [value, address], .. } => match opcode {
                Opcode::AtomicStore => {
                    let address = self.pointer(address)?;
                    let stored = self.builder.build_store(address, self.plain(value)?)?;
                    Self::make_atomic(stored)?;
                }
                _ => return Err(Unhandled::Opcode(opcode)),
            },
            InstructionData::AtomicRmw { args: [address, operand], op, .. } => {
                let address = self.pointer(address)?;
                let operand = self.int(operand)?;
                let op = match op {
                    AtomicRmwOp::Add => AtomicRMWBinOp::Add,
                    AtomicRmwOp::Sub => AtomicRMWBinOp::Sub,
                    AtomicRmwOp::And => AtomicRMWBinOp::And,
                    AtomicRmwOp::Nand => AtomicRMWBinOp::Nand,
                    AtomicRmwOp::Or => AtomicRMWBinOp::Or,
                    AtomicRmwOp::Xor => AtomicRMWBinOp::Xor,
                    AtomicRmwOp::Xchg => AtomicRMWBinOp::Xchg,
                    AtomicRmwOp::Umin => AtomicRMWBinOp::UMin,
                    AtomicRmwOp::Umax => AtomicRMWBinOp::UMax,
                    AtomicRmwOp::Smin => AtomicRMWBinOp::Min,
                    AtomicRmwOp::Smax => AtomicRMWBinOp::Max,
                };
                let ordering = AtomicOrdering::SequentiallyConsistent;
                let old = self.builder.build_atomicrmw(op, address, operand, ordering)?;
                self.set(inst, Operand::Int(old));
            }
            InstructionData::AtomicCas { args: [address, expected, replacement], .. } => {
                let address = self.pointer(address)?;
                let (expected, replacement) = (self.int(expected)?, self.int(replacement)?);
                let ordering = AtomicOrdering::SequentiallyConsistent;
                let exchanged = self.builder.build_cmpxchg(
                    address,
                    expected,
                    replacement,
                    ordering,
                    ordering,
                )?;
                let old = self.builder.build_extract_value(exchanged, 0, "")?;
                self.set(inst, old);
            }
            InstructionData::NullAry { .. } => match opcode {
                Opcode::Fence => {
                    self.builder.build_fence(AtomicOrdering::SequentiallyConsistent, false, "")?;
                }
                Opcode::Nop => {}
                // Where the exception arrives in a register, the frame pointer is read for
                // what the caller left above the frame, so it is the one of LLVM's frame.
                Opcode::GetFramePointer if crate::Target::current().exception_in_register() => {
                    use crate::target::{Arch, ObjectFormat};
                    let target = crate::Target::current();
                    let pointer = self.context.ptr_type(AddressSpace::default());
                    let frame = if (target.arch, target.object_format)
                        == (Arch::Aarch64, ObjectFormat::Elf)
                    {
                        // What is asked for is where Cranelift has the frame pointer and the
                        // return address: right below what the caller left on the stack.
                        // LLVM has the registers it saves between the two here.
                        let entry = self.intrinsic("llvm.sponentry", &[pointer.into()], &[])?;
                        let below = self.immediate(self.context.i64_type(), -16, true);
                        self.offset(entry.into_pointer_value(), below)?
                    } else {
                        let own = self.context.i32_type().const_zero();
                        self.intrinsic("llvm.frameaddress", &[pointer.into()], &[own.into()])?
                            .into_pointer_value()
                    };
                    self.set(inst, Operand::Pointer(frame));
                }
                // Asked for by a guarded call alone, to look at the slot of the frame its
                // exception arrives in. LLVM lays out the frame, so the slot is one of the
                // function's own, and this is where it would be the slot of the frame.
                //
                // Where exceptions arrive in a register, it is asked for by a function
                // that takes `...`, to find the arguments on the stack: Cranelift's frame
                // is what says where they are from it.
                Opcode::GetFramePointer => {
                    let target = crate::Target::selected().map_err(Unhandled::Llvm)?;
                    if target.exception_in_register() {
                        return Err(Unhandled::Shape("the frame pointer"));
                    }
                    let arrival = self.arrival()?;
                    let above = self.immediate(self.context.i64_type(), -i64::from(ARRIVAL), true);
                    self.set(inst, Operand::Pointer(self.offset(arrival, above)?));
                }
                _ => return Err(Unhandled::Opcode(opcode)),
            },
            _ => return Err(Unhandled::Opcode(opcode)),
        }
        Ok(())
    }

    /// Where the memory of parameter `at`, which the caller wrote on the stack, is.
    ///
    /// By the Windows convention LLVM takes a `byval` parameter to be a pointer to a copy,
    /// where Cranelift takes it to be the bytes themselves among the arguments on the
    /// stack. Those start past the return address and the room for the four that come in
    /// registers, and this looks no further than the last parameter after ones that take a
    /// slot each.
    fn stack_argument(&self, at: usize) -> Lowered<PointerValue<'ctx>> {
        const REGISTERS: usize = 4;
        let params = &self.func.signature.params;
        let slots = params[..at].iter().all(|it| {
            matches!(it.purpose, ArgumentPurpose::Normal | ArgumentPurpose::StructReturn)
                && it.value_type.bytes() <= 8
        });
        if at < REGISTERS || at + 1 != params.len() || !slots {
            return Err(Unhandled::Shape("memory among the arguments on the stack"));
        }
        let pointer = self.context.ptr_type(AddressSpace::default());
        let returns = self.intrinsic("llvm.addressofreturnaddress", &[pointer.into()], &[])?;
        let past = self.immediate(self.context.i64_type(), 8 * (1 + at as i64), true);
        self.offset(returns.into_pointer_value(), past)
    }

    fn make_atomic(access: InstructionValue<'ctx>) -> Lowered<()> {
        access
            .set_atomic_ordering(AtomicOrdering::SequentiallyConsistent)
            .map_err(|e| Unhandled::Llvm(e.to_string()))
    }

    fn trap(&self) -> Lowered<()> {
        let trap = Intrinsic::find("llvm.trap")
            .and_then(|it| it.get_declaration(self.module, &[]))
            .ok_or_else(|| Unhandled::Llvm("no intrinsic `llvm.trap`".to_owned()))?;
        self.builder.build_call(trap, &[], "")?;
        self.builder.build_unreachable()?;
        Ok(())
    }

    fn unary(&mut self, inst: Inst, opcode: Opcode, arg: Value) -> Lowered<()> {
        let to = self.func.dfg.ctrl_typevar(inst);
        let result_ty = || self.func.dfg.value_type(self.func.dfg.first_result(inst));
        let b = &self.builder;
        let value: Operand<'ctx> = match opcode {
            Opcode::Ineg => Operand::Int(b.build_int_neg(self.int(arg)?, "")?),
            Opcode::Bnot => Operand::Int(b.build_not(self.int(arg)?, "")?),
            Opcode::Uextend => {
                Operand::Int(b.build_int_z_extend(self.int(arg)?, self.int_ty(result_ty())?, "")?)
            }
            Opcode::Sextend => {
                Operand::Int(b.build_int_s_extend(self.int(arg)?, self.int_ty(result_ty())?, "")?)
            }
            Opcode::Ireduce => {
                Operand::Int(b.build_int_truncate(self.int(arg)?, self.int_ty(result_ty())?, "")?)
            }
            Opcode::Clz | Opcode::Ctz => {
                let name = if opcode == Opcode::Clz { "llvm.ctlz" } else { "llvm.cttz" };
                let int = self.int(arg)?;
                // Zero has as many zero bits as it has bits.
                let zero_is_poison = self.context.bool_type().const_zero();
                let args = [int.into(), zero_is_poison.into()];
                self.intrinsic(name, &[int.get_type().into()], &args)?.into()
            }
            Opcode::VhighBits => {
                let Operand::Vector(vector) = self.operand(arg) else {
                    return Err(Unhandled::Type(self.func.dfg.value_type(arg)));
                };
                let zero = vector.get_type().const_zero();
                let negative = b.build_int_compare(IntPredicate::SLT, vector, zero, "")?;
                let lanes = Self::int_of_width(self.context, vector.get_type().get_size())?;
                let bits = b.build_bit_cast(negative, lanes, "")?.into_int_value();
                let to = self.int_ty(result_ty())?;
                Operand::Int(b.build_int_cast_sign_flag(bits, to, false, "")?)
            }
            Opcode::Popcnt => Operand::Int(self.int_intrinsic("llvm.ctpop", &[self.int(arg)?])?),
            Opcode::Bitrev => {
                Operand::Int(self.int_intrinsic("llvm.bitreverse", &[self.int(arg)?])?)
            }
            Opcode::Bswap => Operand::Int(self.int_intrinsic("llvm.bswap", &[self.int(arg)?])?),
            Opcode::Isplit => {
                let wide = self.int(arg)?;
                let half = self.int_ty(to.half_width().ok_or(Unhandled::Type(to))?)?;
                let low = b.build_int_truncate(wide, half, "")?;
                let by = wide.get_type().const_int(half.get_bit_width().into(), false);
                let high = b.build_right_shift(wide, by, false, "")?;
                let high = b.build_int_truncate(high, half, "")?;
                let results = self.func.dfg.inst_results(inst);
                self.values.insert(results[0], Operand::Int(low));
                self.values.insert(results[1], Operand::Int(high));
                return Ok(());
            }
            Opcode::Fneg => Operand::Float(b.build_float_neg(self.float(arg)?, "")?),
            Opcode::Fabs => Operand::Float(self.float_intrinsic("llvm.fabs", &[self.float(arg)?])?),
            Opcode::Sqrt => Operand::Float(self.float_intrinsic("llvm.sqrt", &[self.float(arg)?])?),
            Opcode::Ceil => Operand::Float(self.float_intrinsic("llvm.ceil", &[self.float(arg)?])?),
            Opcode::Floor => {
                Operand::Float(self.float_intrinsic("llvm.floor", &[self.float(arg)?])?)
            }
            Opcode::Trunc => {
                Operand::Float(self.float_intrinsic("llvm.trunc", &[self.float(arg)?])?)
            }
            // Ties go to the even neighbour.
            Opcode::Nearest => {
                Operand::Float(self.float_intrinsic("llvm.roundeven", &[self.float(arg)?])?)
            }
            Opcode::Fpromote | Opcode::Fdemote => {
                let BasicTypeEnum::FloatType(ty) = self.ty(result_ty())? else {
                    return Err(Unhandled::Type(result_ty()));
                };
                Operand::Float(b.build_float_cast(self.float(arg)?, ty, "")?)
            }
            Opcode::FcvtFromUint | Opcode::FcvtFromSint => {
                let BasicTypeEnum::FloatType(ty) = self.ty(result_ty())? else {
                    return Err(Unhandled::Type(result_ty()));
                };
                let int = self.int(arg)?;
                Operand::Float(match opcode {
                    Opcode::FcvtFromUint => b.build_unsigned_int_to_float(int, ty, "")?,
                    _ => b.build_signed_int_to_float(int, ty, "")?,
                })
            }
            Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => {
                let name = match opcode {
                    Opcode::FcvtToUintSat => "llvm.fptoui.sat",
                    _ => "llvm.fptosi.sat",
                };
                let float = self.float(arg)?;
                let types = [self.ty(result_ty())?, float.get_type().into()];
                self.intrinsic(name, &types, &[float.into()])?.into()
            }
            _ => return Err(Unhandled::Opcode(opcode)),
        };
        self.set(inst, value);
        Ok(())
    }

    fn bitcast(&mut self, inst: Inst, arg: Value) -> Lowered<()> {
        let to = self.ty(self.func.dfg.value_type(self.func.dfg.first_result(inst)))?;
        let cast = self.builder.build_bit_cast(self.plain(arg)?, to, "")?;
        self.set(inst, cast);
        Ok(())
    }

    fn binary(&mut self, inst: Inst, opcode: Opcode, l: Value, r: Value) -> Lowered<()> {
        if let Some(&(slot, _)) = self.realigned.get(&inst) {
            self.set(inst, Operand::Pointer(self.slots[&slot]));
            return Ok(());
        }
        let b = &self.builder;
        if self.func.dfg.value_type(l).is_float() {
            let (l, r) = (self.float(l)?, self.float(r)?);
            let value = match opcode {
                Opcode::Fadd => b.build_float_add(l, r, "")?,
                Opcode::Fsub => b.build_float_sub(l, r, "")?,
                Opcode::Fmul => b.build_float_mul(l, r, "")?,
                Opcode::Fdiv => b.build_float_div(l, r, "")?,
                Opcode::Fmin => self.float_intrinsic("llvm.minimum", &[l, r])?,
                Opcode::Fmax => self.float_intrinsic("llvm.maximum", &[l, r])?,
                Opcode::Fcopysign => self.float_intrinsic("llvm.copysign", &[l, r])?,
                _ => return Err(Unhandled::Opcode(opcode)),
            };
            self.set(inst, Operand::Float(value));
            return Ok(());
        }
        if let (Operand::Vector(l), Operand::Vector(r)) = (self.operand(l), self.operand(r)) {
            let saturating = |name: &str| -> Lowered<VectorValue<'ctx>> {
                let args = [l.into(), r.into()];
                Ok(self.intrinsic(name, &[l.get_type().into()], &args)?.into_vector_value())
            };
            let value = match opcode {
                Opcode::Iadd => b.build_int_add(l, r, "")?,
                Opcode::Isub => b.build_int_sub(l, r, "")?,
                Opcode::Band => b.build_and(l, r, "")?,
                Opcode::Bor => b.build_or(l, r, "")?,
                Opcode::Bxor => b.build_xor(l, r, "")?,
                Opcode::UaddSat => saturating("llvm.uadd.sat")?,
                Opcode::SaddSat => saturating("llvm.sadd.sat")?,
                Opcode::UsubSat => saturating("llvm.usub.sat")?,
                Opcode::SsubSat => saturating("llvm.ssub.sat")?,
                // The pairs of `l` and then of `r`, which AArch64 selects as one `addp`.
                Opcode::IaddPairwise => {
                    let index = self.context.i32_type();
                    let lanes = l.get_type().get_size();
                    let mask = |first: u32| {
                        let lanes: Vec<IntValue<'ctx>> = (0..lanes)
                            .map(|lane| index.const_int(u64::from(lane * 2 + first), false))
                            .collect();
                        VectorType::const_vector(&lanes)
                    };
                    let even = b.build_shuffle_vector(l, r, mask(0), "")?;
                    let odd = b.build_shuffle_vector(l, r, mask(1), "")?;
                    b.build_int_add(even, odd, "")?
                }
                _ => return Err(Unhandled::Opcode(opcode)),
            };
            self.set(inst, Operand::Vector(value));
            return Ok(());
        }
        // An `offset` is of an address, whether or not it was loaded as an integer.
        if opcode == Opcode::Iadd && unwind::seh::is_in_bounds(self.func.srcloc(inst)) {
            let (base, by) = (self.pointer(l)?, self.int(r)?);
            // SAFETY: `offset` stays within what `base` points into.
            let address =
                unsafe { b.build_in_bounds_gep(self.context.i8_type(), base, &[by], "")? };
            self.set(inst, Operand::Pointer(address));
            return Ok(());
        }
        // An offset address is still an address of the same thing.
        match (opcode, self.operand(l), self.operand(r)) {
            (Opcode::Iadd, Operand::Pointer(base), Operand::Int(by))
            | (Opcode::Iadd, Operand::Int(by), Operand::Pointer(base)) => {
                let address = self.offset(base, by)?;
                self.set(inst, Operand::Pointer(address));
                return Ok(());
            }
            _ => {}
        }
        let (l, r) = (self.int(l)?, self.int(r)?);
        let ty = l.get_type();
        // Cranelift takes a shift amount of any integer type, modulo the width shifted.
        let amount = |r: IntValue<'ctx>| -> Lowered<IntValue<'ctx>> {
            let r = b.build_int_cast_sign_flag(r, ty, false, "")?;
            Ok(b.build_and(r, ty.const_int(u64::from(ty.get_bit_width()) - 1, false), "")?)
        };
        let overflowing = |name: &str| -> Lowered<(IntValue<'ctx>, IntValue<'ctx>)> {
            let pair = self.intrinsic(name, &[ty.into()], &[l.into(), r.into()])?;
            let pair = pair.into_struct_value();
            let value = b.build_extract_value(pair, 0, "")?.into_int_value();
            let overflowed = b.build_extract_value(pair, 1, "")?.into_int_value();
            Ok((value, overflowed))
        };
        let value = match opcode {
            Opcode::Iadd => b.build_int_add(l, r, "")?,
            Opcode::Isub => b.build_int_sub(l, r, "")?,
            Opcode::Imul => b.build_int_mul(l, r, "")?,
            Opcode::Udiv => b.build_int_unsigned_div(l, r, "")?,
            Opcode::Sdiv => b.build_int_signed_div(l, r, "")?,
            Opcode::Urem => b.build_int_unsigned_rem(l, r, "")?,
            Opcode::Srem => b.build_int_signed_rem(l, r, "")?,
            Opcode::Band => b.build_and(l, r, "")?,
            Opcode::Bor => b.build_or(l, r, "")?,
            Opcode::Bxor => b.build_xor(l, r, "")?,
            Opcode::Ishl => b.build_left_shift(l, amount(r)?, "")?,
            Opcode::Ushr => b.build_right_shift(l, amount(r)?, false, "")?,
            Opcode::Sshr => b.build_right_shift(l, amount(r)?, true, "")?,
            Opcode::Rotl | Opcode::Rotr => {
                let name = if opcode == Opcode::Rotl { "llvm.fshl" } else { "llvm.fshr" };
                self.int_intrinsic(name, &[l, l, amount(r)?])?
            }
            Opcode::Umin => self.int_intrinsic("llvm.umin", &[l, r])?,
            Opcode::Umax => self.int_intrinsic("llvm.umax", &[l, r])?,
            Opcode::Smin => self.int_intrinsic("llvm.smin", &[l, r])?,
            Opcode::Smax => self.int_intrinsic("llvm.smax", &[l, r])?,
            Opcode::Umulhi | Opcode::Smulhi => {
                let signed = opcode == Opcode::Smulhi;
                let wide = Self::int_of_width(self.context, ty.get_bit_width() * 2)?;
                let l = b.build_int_cast_sign_flag(l, wide, signed, "")?;
                let r = b.build_int_cast_sign_flag(r, wide, signed, "")?;
                let product = b.build_int_mul(l, r, "")?;
                let by = wide.const_int(ty.get_bit_width().into(), false);
                b.build_int_truncate(b.build_right_shift(product, by, false, "")?, ty, "")?
            }
            Opcode::Iconcat => {
                let wide = Self::int_of_width(self.context, ty.get_bit_width() * 2)?;
                let low = b.build_int_z_extend(l, wide, "")?;
                let high = b.build_int_z_extend(r, wide, "")?;
                let by = wide.const_int(ty.get_bit_width().into(), false);
                b.build_or(low, b.build_left_shift(high, by, "")?, "")?
            }
            Opcode::UaddOverflow
            | Opcode::SaddOverflow
            | Opcode::UsubOverflow
            | Opcode::SsubOverflow
            | Opcode::UmulOverflow
            | Opcode::SmulOverflow => {
                // An unsigned sum or difference is checked by comparing, as rustc does: what
                // LLVM knows of comparisons (that `x < len` in a loop, say) then removes the
                // check, which it does not for the intrinsics.
                let (value, overflowed) = match opcode {
                    Opcode::UaddOverflow => {
                        let sum = b.build_int_add(l, r, "")?;
                        (sum, b.build_int_compare(IntPredicate::ULT, sum, l, "")?)
                    }
                    Opcode::UsubOverflow => (
                        b.build_int_sub(l, r, "")?,
                        b.build_int_compare(IntPredicate::ULT, l, r, "")?,
                    ),
                    _ => overflowing(match opcode {
                        Opcode::SaddOverflow => "llvm.sadd.with.overflow",
                        Opcode::SsubOverflow => "llvm.ssub.with.overflow",
                        Opcode::UmulOverflow => "llvm.umul.with.overflow",
                        _ => "llvm.smul.with.overflow",
                    })?,
                };
                let results = self.func.dfg.inst_results(inst);
                let byte = b.build_int_z_extend(overflowed, self.context.i8_type(), "")?;
                self.values.insert(results[0], Operand::Int(value));
                self.values.insert(results[1], Operand::Int(byte));
                self.conditions.insert(results[1], overflowed);
                return Ok(());
            }
            _ => return Err(Unhandled::Opcode(opcode)),
        };
        self.set(inst, Operand::Int(value));
        Ok(())
    }

    fn int_predicate(cond: IntCC) -> IntPredicate {
        match cond {
            IntCC::Equal => IntPredicate::EQ,
            IntCC::NotEqual => IntPredicate::NE,
            IntCC::SignedLessThan => IntPredicate::SLT,
            IntCC::SignedGreaterThanOrEqual => IntPredicate::SGE,
            IntCC::SignedGreaterThan => IntPredicate::SGT,
            IntCC::SignedLessThanOrEqual => IntPredicate::SLE,
            IntCC::UnsignedLessThan => IntPredicate::ULT,
            IntCC::UnsignedGreaterThanOrEqual => IntPredicate::UGE,
            IntCC::UnsignedGreaterThan => IntPredicate::UGT,
            IntCC::UnsignedLessThanOrEqual => IntPredicate::ULE,
        }
    }

    fn float_predicate(cond: FloatCC) -> FloatPredicate {
        match cond {
            FloatCC::Ordered => FloatPredicate::ORD,
            FloatCC::Unordered => FloatPredicate::UNO,
            FloatCC::Equal => FloatPredicate::OEQ,
            FloatCC::NotEqual => FloatPredicate::UNE,
            FloatCC::OrderedNotEqual => FloatPredicate::ONE,
            FloatCC::UnorderedOrEqual => FloatPredicate::UEQ,
            FloatCC::LessThan => FloatPredicate::OLT,
            FloatCC::LessThanOrEqual => FloatPredicate::OLE,
            FloatCC::GreaterThan => FloatPredicate::OGT,
            FloatCC::GreaterThanOrEqual => FloatPredicate::OGE,
            FloatCC::UnorderedOrLessThan => FloatPredicate::ULT,
            FloatCC::UnorderedOrLessThanOrEqual => FloatPredicate::ULE,
            FloatCC::UnorderedOrGreaterThan => FloatPredicate::UGT,
            FloatCC::UnorderedOrGreaterThanOrEqual => FloatPredicate::UGE,
        }
    }

    /// Cranelift says nothing about how an address is aligned unless the access is flagged.
    fn set_access_alignment(access: InstructionValue<'ctx>, aligned: bool) -> Lowered<()> {
        if aligned {
            return Ok(());
        }
        access.set_alignment(1).map_err(|e| Unhandled::Llvm(e.to_string()))
    }

    fn load(
        &mut self,
        inst: Inst,
        opcode: Opcode,
        base: Value,
        offset: i32,
        aligned: bool,
    ) -> Lowered<()> {
        let result = self.func.dfg.value_type(self.func.dfg.first_result(inst));
        let (narrow, signed) = match opcode {
            Opcode::Load => (result, false),
            Opcode::Uload8 => (types::I8, false),
            Opcode::Sload8 => (types::I8, true),
            Opcode::Uload16 => (types::I16, false),
            Opcode::Sload16 => (types::I16, true),
            Opcode::Uload32 => (types::I32, false),
            Opcode::Sload32 => (types::I32, true),
            _ => return Err(Unhandled::Opcode(opcode)),
        };
        let address = self.pointerish.contains(&self.func.dfg.first_result(inst));
        let ty = match address && narrow == result {
            true => self.context.ptr_type(AddressSpace::default()).into(),
            false => self.ty(narrow)?,
        };
        let loaded = match self.known(base, offset, narrow) {
            Some(known) => known,
            None => {
                let address = self.address(base, offset)?;
                let loaded = self.builder.build_load(ty, address, "")?;
                Self::set_access_alignment(
                    loaded.as_instruction_value().expect("a load"),
                    aligned,
                )?;
                loaded
            }
        };
        if narrow == result {
            self.set(inst, loaded);
            return Ok(());
        }
        let wide = self.int_ty(result)?;
        let extended =
            self.builder.build_int_cast_sign_flag(loaded.into_int_value(), wide, signed, "")?;
        self.set(inst, Operand::Int(extended));
        Ok(())
    }

    fn store(
        &mut self,
        opcode: Opcode,
        value: Value,
        address: PointerValue<'ctx>,
        aligned: bool,
    ) -> Lowered<()> {
        let narrow = match opcode {
            Opcode::Store => None,
            Opcode::Istore8 => Some(types::I8),
            Opcode::Istore16 => Some(types::I16),
            Opcode::Istore32 => Some(types::I32),
            _ => return Err(Unhandled::Opcode(opcode)),
        };
        let value = match narrow {
            // An address is stored as the pointer it is, so that a load of it gives it back.
            None => self.basic(value),
            Some(ty) => {
                self.builder.build_int_truncate(self.int(value)?, self.int_ty(ty)?, "")?.into()
            }
        };
        let stored = self.builder.build_store(address, value)?;
        Self::set_access_alignment(stored, aligned)
    }

    /// A library call that copies or fills memory, as LLVM's intrinsic: a call LLVM knows
    /// nothing of, of a size it does know, is not written as the loads and stores it is.
    fn memory(&mut self, libcall: LibCall, args: &[Value]) -> Lowered<PointerValue<'ctx>> {
        let &[to, from, size] = args else {
            return Err(Unhandled::Shape("a memory call without three arguments"));
        };
        let (to, size) = (self.pointer(to)?, self.int(size)?);
        match libcall {
            LibCall::Memcpy => self.builder.build_memcpy(to, 1, self.pointer(from)?, 1, size)?,
            LibCall::Memmove => self.builder.build_memmove(to, 1, self.pointer(from)?, 1, size)?,
            _ => {
                let byte = self.context.i8_type();
                let byte = self.builder.build_int_truncate(self.int(from)?, byte, "")?;
                self.builder.build_memset(to, 1, byte, size)?
            }
        };
        Ok(to)
    }

    /// What a function or data object is called in the module: a request by the name it is
    /// found again by in the object LLVM writes, a library function by its own.
    fn external_name(&self, name: &ExternalName) -> Lowered<String> {
        match name {
            ExternalName::User(name) => {
                let name = &self.func.params.user_named_funcs()[*name];
                Ok(super::Symbol::of_user(name.namespace, name.index).to_string())
            }
            ExternalName::LibCall(libcall) => {
                Ok(cranelift_module::default_libcall_names()(*libcall))
            }
            _ => Err(Unhandled::Shape("a name that is neither a request nor a library call")),
        }
    }

    /// The `asm!` block the call of `func_ref` runs, when it goes where it is called.
    fn asm_block(&self, func_ref: ir::FuncRef) -> Option<Arc<asm::Inline>> {
        let name = self.external_name(&self.func.dfg.ext_funcs[func_ref].name).ok()?;
        match super::Symbol::parse(&name)? {
            super::Symbol::Func(id) => self.blocks_asm.get(&id).cloned(),
            super::Symbol::Data(_) => None,
        }
    }

    /// `block` where its wrapper is called with `slot`: the inputs read from the slot, the
    /// outputs written to it.
    fn inline_asm(&mut self, block: &asm::Inline, slot: Value) -> Lowered<()> {
        let base = self.pointer(slot)?;
        let context = self.context;
        let int = |size: u32| match size {
            1 => Ok(context.i8_type()),
            2 => Ok(context.i16_type()),
            4 => Ok(context.i32_type()),
            8 => Ok(context.i64_type()),
            _ => Err(Unhandled::Shape("an `asm!` operand of an odd size")),
        };
        let at = |lowering: &Self, offset: u32| {
            let offset = lowering.context.i64_type().const_int(offset.into(), false);
            // SAFETY: the offset is within the slot, which the wrapper's offsets are of.
            unsafe { lowering.builder.build_gep(lowering.context.i8_type(), base, &[offset], "") }
        };
        let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
        let mut params: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::new();
        for &(offset, size) in &block.inputs {
            let address = at(self, offset)?;
            args.push(self.builder.build_load(int(size)?, address, "")?.into());
            params.push(int(size)?.into());
        }
        let outputs: Vec<BasicTypeEnum<'ctx>> =
            block.outputs.iter().map(|&(_, size)| Ok(int(size)?.into())).collect::<Lowered<_>>()?;
        let ty = match outputs.as_slice() {
            [] => self.context.void_type().fn_type(&params, false),
            [one] => one.fn_type(&params, false),
            more => self.context.struct_type(more, false).fn_type(&params, false),
        };
        let options = block.options;
        let intel = crate::Target::selected()
            .is_ok_and(|it| it.arch == crate::target::Arch::X86_64)
            && !options.contains(AsmOptions::ATT_SYNTAX);
        let pure = options.contains(AsmOptions::PURE);
        let code = self.context.create_inline_asm(
            ty,
            block.text.clone(),
            block.constraints.clone(),
            !pure,
            !options.contains(AsmOptions::NOSTACK),
            intel.then_some(InlineAsmDialect::Intel),
            false,
        );
        let call = self.builder.build_indirect_call(ty, code, &args, "")?;
        let nounwind = super::attribute_kind("nounwind");
        call.add_attribute(AttributeLoc::Function, self.context.create_enum_attribute(nounwind, 0));
        if options.contains(AsmOptions::NOMEM) {
            let memory = super::attribute_kind("memory");
            call.add_attribute(
                AttributeLoc::Function,
                self.context.create_enum_attribute(memory, 0),
            );
        }
        let returned = call.try_as_basic_value().basic();
        for (at_index, &(offset, size)) in block.outputs.iter().enumerate() {
            let Some(offset) = offset else { continue };
            let value = match (block.outputs.len(), returned) {
                (1, Some(value)) => value,
                (_, Some(BasicValueEnum::StructValue(all))) => {
                    self.builder.build_extract_value(all, at_index as u32, "")?
                }
                _ => return Err(Unhandled::Shape("an `asm!` block without its outputs")),
            };
            debug_assert_eq!(value.get_type(), int(size)?.into());
            let address = at(self, offset)?;
            self.builder.build_store(address, value)?;
        }
        Ok(())
    }

    fn callee(&mut self, func_ref: ir::FuncRef) -> Lowered<FunctionValue<'ctx>> {
        if let Some(&callee) = self.callees.get(&func_ref) {
            return Ok(callee);
        }
        let data = &self.func.dfg.ext_funcs[func_ref];
        let mut name = self.external_name(&data.name)?;
        let sig = &self.func.dfg.signatures[data.signature];
        let ty = Self::function_type(self.context, sig)?;
        let shown = match super::Symbol::parse(&name) {
            Some(super::Symbol::Func(id)) if Some(id) == self.memcmp => {
                name = "memcmp".to_owned();
                None
            }
            Some(super::Symbol::Func(id)) => self.shown.get(&id).copied(),
            _ => None,
        };
        // One request can be imported with more than one signature; calls then go through
        // the pointer with their own.
        let callee = match shown.or_else(|| self.module.get_function(&name)) {
            Some(known) if !Self::same_but_pointers(known.get_type(), ty) => {
                return Err(Unhandled::Shape("one callee with two signatures"));
            }
            Some(known) => known,
            None => {
                let pointers = match name.as_str() {
                    "memcmp" => &[0, 1][..],
                    _ => self.pointer_params.get(&name).map_or(&[][..], Vec::as_slice),
                };
                let ty = Self::with_pointers(self.context, ty, pointers);
                let callee = self.module.add_function(&name, ty, Some(Linkage::External));
                Self::mark_signature(self.context, sig, Side::Caller, |loc, attribute| {
                    callee.add_attribute(loc, attribute)
                });
                callee
            }
        };
        self.callees.insert(func_ref, callee);
        Ok(callee)
    }

    fn global(&mut self, global: ir::GlobalValue) -> Lowered<PointerValue<'ctx>> {
        if let Some(&known) = self.globals.get(&global) {
            return Ok(known);
        }
        let GlobalValueData::Symbol { name, offset, colocated, tls } =
            &self.func.global_values[global]
        else {
            return Err(Unhandled::Shape("a global value that is not a symbol"));
        };
        if offset.bits() != 0 {
            return Err(Unhandled::Shape("a symbol with an offset"));
        }
        let name = self.external_name(name)?;
        let data = match self.module.get_global(&name) {
            Some(known) => known,
            None => {
                // Two statics can be one object, when one is an `extern` static that links to
                // the other: LLVM takes two globals for two objects and folds a comparison of
                // their addresses to false. One of a type without a size may be at any
                // address, another's too.
                let is_static = matches!(
                    super::Symbol::parse(&name),
                    Some(super::Symbol::Data(index)) if self.statics.contains(&index)
                );
                let data = if is_static {
                    let ty = self
                        .context
                        .get_struct_type(STATIC)
                        .unwrap_or_else(|| self.context.opaque_struct_type(STATIC));
                    self.module.add_global(ty, None, &name)
                } else {
                    self.module.add_global(self.context.i8_type(), None, &name)
                };
                data.set_linkage(Linkage::External);
                // Hidden is how LLVM is told the object is in this program, and so reached
                // without the GOT, as Cranelift reaches it.
                if *colocated {
                    data.set_visibility(GlobalVisibility::Hidden);
                }
                if *tls {
                    data.set_thread_local(true);
                    data.set_thread_local_mode(Some(ThreadLocalMode::GeneralDynamicTLSModel));
                }
                data
            }
        };
        let pointer = data.as_pointer_value();
        self.globals.insert(global, pointer);
        Ok(pointer)
    }

    /// What a load of `ty` at `offset` from `base` gives, where `base` is a data object
    /// whose bytes are known and do not change. LLVM is given the value and not the load:
    /// a load that feeds only a variable's slot is dropped with the slot, and a debugger
    /// then has nothing to show for the variable, while a constant stays told.
    fn known(&self, base: Value, offset: i32, ty: Type) -> Option<BasicValueEnum<'ctx>> {
        let ir::ValueDef::Result(inst, _) = self.func.dfg.value_def(base) else { return None };
        let InstructionData::UnaryGlobalValue { global_value, .. } = self.func.dfg.insts[inst]
        else {
            return None;
        };
        let GlobalValueData::Symbol { name, .. } = &self.func.global_values[global_value] else {
            return None;
        };
        let name = self.external_name(name).ok()?;
        let Some(super::Symbol::Data(index)) = super::Symbol::parse(&name) else { return None };
        let from = usize::try_from(offset).ok()?;
        let bytes = self.constants.get(&index)?.get(from..from + ty.bytes() as usize)?;
        let words: Vec<u64> = bytes
            .chunks(8)
            .map(|word| word.iter().rev().fold(0, |all, &byte| all << 8 | u64::from(byte)))
            .collect();
        let bits = self.context.custom_width_int_type(std::num::NonZero::new(ty.bits())?).ok()?;
        let value = bits.const_int_arbitrary_precision(&words);
        match self.ty(ty).ok()? {
            BasicTypeEnum::IntType(_) => Some(value.into()),
            BasicTypeEnum::FloatType(float) => self.builder.build_bit_cast(value, float, "").ok(),
            _ => None,
        }
    }

    fn arguments(
        &self,
        ty: FunctionType<'ctx>,
        sig: &Signature,
        args: &[Value],
    ) -> Lowered<Vec<BasicMetadataValueEnum<'ctx>>> {
        Ok(self.invoke_arguments(ty, sig, args)?.into_iter().map(Into::into).collect())
    }

    /// What a call of type `ty` passes for `args`: what is a pointer there is passed as one.
    fn invoke_arguments(
        &self,
        ty: FunctionType<'ctx>,
        sig: &Signature,
        args: &[Value],
    ) -> Lowered<Vec<BasicValueEnum<'ctx>>> {
        let types = ty.get_param_types();
        std::iter::zip(&sig.params, args)
            .enumerate()
            .map(|(at, (param, &arg))| {
                let pointer = types.get(at).is_some_and(|it| it.is_pointer_type());
                Ok(match param.purpose {
                    ArgumentPurpose::Normal if !pointer => self.plain(arg)?,
                    _ => self.pointer(arg)?.into(),
                })
            })
            .collect()
    }

    /// The type of an indirect call of `sig` with `args`: an argument that is an address is
    /// passed as a pointer, which does not have it escape before the call.
    fn indirect_type(&self, sig: &Signature, args: &[Value]) -> Lowered<FunctionType<'ctx>> {
        let ty = Self::function_type(self.context, sig)?;
        let pointers: Vec<u32> = std::iter::zip(&sig.params, args)
            .enumerate()
            .filter(|(_, (param, arg))| {
                param.purpose == ArgumentPurpose::Normal
                    && matches!(self.operand(**arg), Operand::Pointer(_))
            })
            .map(|(at, _)| at as u32)
            .collect();
        Ok(Self::with_pointers(self.context, ty, &pointers))
    }

    /// The parameters of each callee that some call of it passes an address to, by its name:
    /// the callee is declared with pointers there.
    fn pointer_params(&self, pointerish: &FxHashSet<Value>) -> FxHashMap<String, Vec<u32>> {
        let dfg = &self.func.dfg;
        let mut found: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for block in self.func.layout.blocks() {
            for inst in self.func.layout.block_insts(block) {
                let func_ref = match dfg.insts[inst] {
                    InstructionData::Call { func_ref, .. }
                    | InstructionData::TryCall { func_ref, .. } => func_ref,
                    _ => continue,
                };
                let data = &dfg.ext_funcs[func_ref];
                let Ok(name) = self.external_name(&data.name) else { continue };
                let sig = &dfg.signatures[data.signature];
                let args = std::iter::zip(&sig.params, dfg.inst_args(inst)).enumerate();
                for (at, (param, arg)) in args {
                    if param.purpose == ArgumentPurpose::Normal
                        && pointerish.contains(&dfg.resolve_aliases(*arg))
                    {
                        let positions = found.entry(name.clone()).or_default();
                        if !positions.contains(&(at as u32)) {
                            positions.push(at as u32);
                        }
                    }
                }
            }
        }
        found
    }

    fn returned(
        &self,
        call: CallSiteValue<'ctx>,
        sig: &Signature,
    ) -> Lowered<Vec<BasicValueEnum<'ctx>>> {
        Self::mark_signature(self.context, sig, Side::Caller, |loc, attribute| {
            call.add_attribute(loc, attribute)
        });
        let value = call.try_as_basic_value().basic();
        Ok(match (sig.returns.len(), value) {
            (0, _) => Vec::new(),
            (1, Some(value)) => vec![value],
            (count, Some(BasicValueEnum::StructValue(pair))) => (0..count as u32)
                .map(|at| self.builder.build_extract_value(pair, at, ""))
                .collect::<Result<_, _>>()?,
            _ => return Err(Unhandled::Shape("a call that returns something else")),
        })
    }

    fn finish_call(
        &mut self,
        call: CallSiteValue<'ctx>,
        sig: &Signature,
        results: &[Value],
    ) -> Lowered<()> {
        for (&result, value) in std::iter::zip(results, self.returned(call, sig)?) {
            self.values.insert(result, value.into());
        }
        Ok(())
    }

    /// The slot a guarded call that unwound leaves its exception in.
    fn arrival(&mut self) -> Lowered<PointerValue<'ctx>> {
        if let Some(known) = self.arrival {
            return Ok(known);
        }
        let at = self.builder.get_insert_block();
        let frame = self.frame.and_then(BasicBlock::get_terminator);
        let frame = frame.ok_or(Unhandled::Shape("a frame pointer ahead of the frame"))?;
        self.builder.position_before(&frame);
        let pointer = self.context.ptr_type(AddressSpace::default());
        let arrival = self.builder.build_alloca(pointer, "")?;
        if let Some(at) = at {
            self.builder.position_at_end(at);
        }
        self.arrival = Some(arrival);
        Ok(arrival)
    }

    /// The blocks a guarded call continues in: where it returns to, and its landing pad.
    fn guard(&self) -> (BasicBlock<'ctx>, BasicBlock<'ctx>) {
        let block = || self.context.append_basic_block(self.function, "");
        (block(), block())
    }

    /// Has a guarded call go on in one block whichever way it came back, as Cranelift's
    /// does: with what it returned, or with the exception in the slot, which is what the
    /// code after the call looks at.
    fn finish_guarded(
        &mut self,
        call: CallSiteValue<'ctx>,
        sig: &Signature,
        results: &[Value],
        returned: BasicBlock<'ctx>,
        pad: BasicBlock<'ctx>,
    ) -> Lowered<()> {
        let context = self.context;
        let after = context.append_basic_block(self.function, "");
        self.builder.position_at_end(returned);
        let values = self.returned(call, sig)?;
        self.builder.build_unconditional_branch(after)?;

        let personality = self.personality(unwind::seh::PERSONALITY);
        let arrival = self.arrival()?;
        self.builder.position_at_end(pad);
        let pointer = context.ptr_type(AddressSpace::default());
        let exception = context.struct_type(&[pointer.into(), context.i32_type().into()], false);
        // The personality enters the pad for whatever is thrown, with no say of the pad's.
        let landed = self.builder.build_landing_pad(exception, personality, &[], true, "")?;
        let payload = self.builder.build_extract_value(landed.into_struct_value(), 0, "")?;
        self.builder.build_store(arrival, payload)?;
        self.builder.build_unconditional_branch(after)?;

        self.builder.position_at_end(after);
        for (&result, value) in std::iter::zip(results, values) {
            let ty = value.get_type();
            let phi = self.builder.build_phi(ty, "")?;
            phi.add_incoming(&[(&value, returned), (&ty.const_zero(), pad)]);
            self.values.insert(result, phi.as_basic_value().into());
        }
        Ok(())
    }

    /// The function the unwinder asks what to do in a frame of this one.
    fn personality(&self, name: &str) -> FunctionValue<'ctx> {
        let personality = self.module.get_function(name).unwrap_or_else(|| {
            let ty = self.context.i32_type().fn_type(&[], true);
            self.module.add_function(name, ty, Some(Linkage::External))
        });
        self.function.set_personality_function(personality);
        personality
    }

    /// The blocks an unwinding call continues in: one of its own that hands what it returned
    /// to the block Cranelift names, and the landing pad of its handler.
    fn exception_edges(
        &mut self,
        table: ir::ExceptionTable,
    ) -> Lowered<(BasicBlock<'ctx>, BasicBlock<'ctx>)> {
        let dfg = &self.func.dfg;
        let table = &dfg.exception_tables[table];
        // Both tags catch every panic, so the first handler is the only one a panic reaches.
        // A call of a body Cranelift built into its caller has its own handler first and the
        // caller's after it.
        let Some((tag, handler)) = table.items().find_map(|item| match item {
            ExceptionTableItem::Tag(tag, handler) => Some((tag, handler)),
            _ => None,
        }) else {
            return Err(Unhandled::Shape("an exception table without a handler"));
        };
        let handler_args: Vec<_> = handler.args(&dfg.value_lists).collect();
        if !matches!(handler_args[..], [BlockArg::TryCallExn(0)]) {
            let pad = self.landing_pad_of_call(tag.as_u32(), handler)?;
            return Ok((self.context.append_basic_block(self.function, ""), pad));
        }
        let pad = Pad { handler: handler.block(&dfg.value_lists), tag: tag.as_u32() };
        let pad = match self.pads.get(&pad) {
            Some(&known) => known,
            None => {
                let block = self.landing_pad(pad)?;
                self.pads.insert(pad, block);
                block
            }
        };
        Ok((self.context.append_basic_block(self.function, ""), pad))
    }

    fn resumes(&self, func_ref: ir::FuncRef) -> bool {
        let name = self.external_name(&self.func.dfg.ext_funcs[func_ref].name);
        let id = name.ok().and_then(|it| super::Symbol::parse(&it));
        matches!(id, Some(super::Symbol::Func(id)) if Some(id) == self.resume)
    }

    /// Goes on unwinding with `exception`, as LLVM's `resume`, which LLVM knows: a landing
    /// pad that does nothing else it drops, and a body built in at a call that unwinds to a
    /// pad goes there instead. The selector is not looked at by `_Unwind_Resume`, which is
    /// what `resume` becomes. What follows, a trap, is left in a block of its own.
    fn resume(&mut self, exception: Value) -> Lowered<()> {
        let context = self.context;
        let pointer = context.ptr_type(AddressSpace::default());
        self.personality(super::PERSONALITY);
        let ty = context.struct_type(&[pointer.into(), context.i32_type().into()], false);
        let exception = self.pointer(exception)?;
        let value = self.builder.build_insert_value(ty.get_undef(), exception, 0, "")?;
        let value =
            self.builder.build_insert_value(value, context.i32_type().const_zero(), 1, "")?;
        self.builder.build_resume(value.into_struct_value())?;
        let after = context.append_basic_block(self.function, "");
        self.builder.position_at_end(after);
        Ok(())
    }

    /// Goes on unwinding with `exception` from a call that unwinds to the handlers of `table`,
    /// which a body Cranelift built in at such a call has: straight to the first of them, as
    /// LLVM does with a `resume` of a body it builds in.
    fn unwound_to(&mut self, table: ir::ExceptionTable, exception: Value) -> Lowered<()> {
        let dfg = &self.func.dfg;
        let Some(handler) = dfg.exception_tables[table].items().find_map(|item| match item {
            ExceptionTableItem::Tag(_, handler) => Some(handler),
            _ => None,
        }) else {
            return Err(Unhandled::Shape("an exception table without a handler"));
        };
        let from = self.builder.get_insert_block().expect("a block being filled");
        let exception = self.int(exception)?;
        let target = handler.block(&dfg.value_lists);
        let args: Vec<BlockArg> = handler.args(&dfg.value_lists).collect();
        let phis = self.phis.get(&target).cloned().unwrap_or_default();
        if phis.len() != args.len() {
            return Err(Unhandled::Shape("a handler given other values than it takes"));
        }
        for (phi, arg) in std::iter::zip(&phis, args) {
            let value: BasicValueEnum<'ctx> = match arg {
                BlockArg::Value(value) => self.passed(*phi, value)?,
                BlockArg::TryCallExn(0) => self.as_phi(*phi, exception.into())?,
                _ => return Err(Unhandled::Shape("a handler that takes what a call returns")),
            };
            phi.add_incoming(&[(&value, from)]);
        }
        self.builder.build_unconditional_branch(self.blocks[&target])?;
        Ok(())
    }

    /// A block every call with this handler unwinds to: takes the exception and goes on to
    /// the handler with it.
    fn landing_pad(&mut self, pad: Pad) -> Lowered<BasicBlock<'ctx>> {
        let context = self.context;
        let pointer = context.ptr_type(AddressSpace::default());
        let personality = self.personality(super::PERSONALITY);

        let resume_at = self.builder.get_insert_block();
        let block = context.append_basic_block(self.function, "");
        self.builder.position_at_end(block);
        let exception = context.struct_type(&[pointer.into(), context.i32_type().into()], false);
        // A null type catches everything.
        let (clauses, cleanup): (&[BasicValueEnum<'ctx>], bool) = match pad.tag {
            unwind::CLEANUP => (&[], true),
            unwind::CATCH => (&[pointer.const_null().into()], false),
            _ => return Err(Unhandled::Shape("an exception tag that is not ours")),
        };
        let landed =
            self.builder.build_landing_pad(exception, personality, clauses, cleanup, "")?;
        let payload = self.builder.build_extract_value(landed.into_struct_value(), 0, "")?;
        let payload =
            self.builder.build_ptr_to_int(payload.into_pointer_value(), context.i64_type(), "")?;
        let Some(&[phi]) = self.phis.get(&pad.handler).map(Vec::as_slice) else {
            return Err(Unhandled::Shape("a handler that takes more than the exception"));
        };
        phi.add_incoming(&[(&self.as_phi(phi, payload.into())?, block)]);
        self.builder.build_unconditional_branch(self.blocks[&pad.handler])?;
        if let Some(resume_at) = resume_at {
            self.builder.position_at_end(resume_at);
        }
        Ok(block)
    }

    /// A landing pad of one call, whose handler takes values of the caller besides the
    /// exception: those are of before the call, and so are there in the pad.
    fn landing_pad_of_call(&mut self, tag: u32, handler: BlockCall) -> Lowered<BasicBlock<'ctx>> {
        let context = self.context;
        let pointer = context.ptr_type(AddressSpace::default());
        let personality = self.personality(super::PERSONALITY);
        let resume_at = self.builder.get_insert_block();
        let block = context.append_basic_block(self.function, "");
        self.builder.position_at_end(block);
        let exception = context.struct_type(&[pointer.into(), context.i32_type().into()], false);
        let (clauses, cleanup): (&[BasicValueEnum<'ctx>], bool) = match tag {
            unwind::CLEANUP => (&[], true),
            unwind::CATCH => (&[pointer.const_null().into()], false),
            _ => return Err(Unhandled::Shape("an exception tag that is not ours")),
        };
        let landed =
            self.builder.build_landing_pad(exception, personality, clauses, cleanup, "")?;
        let payload = self.builder.build_extract_value(landed.into_struct_value(), 0, "")?;
        let payload =
            self.builder.build_ptr_to_int(payload.into_pointer_value(), context.i64_type(), "")?;
        let dfg = &self.func.dfg;
        let target = handler.block(&dfg.value_lists);
        let args: Vec<BlockArg> = handler.args(&dfg.value_lists).collect();
        let phis = self.phis.get(&target).cloned().unwrap_or_default();
        if phis.len() != args.len() {
            return Err(Unhandled::Shape("a handler given other values than it takes"));
        }
        for (phi, arg) in std::iter::zip(&phis, args) {
            let value: BasicValueEnum<'ctx> = match arg {
                BlockArg::Value(value) => self.passed(*phi, value)?,
                BlockArg::TryCallExn(0) => self.as_phi(*phi, payload.into())?,
                _ => return Err(Unhandled::Shape("a handler that takes what a call returns")),
            };
            phi.add_incoming(&[(&value, block)]);
        }
        self.builder.build_unconditional_branch(self.blocks[&target])?;
        if let Some(resume_at) = resume_at {
            self.builder.position_at_end(resume_at);
        }
        Ok(block)
    }

    fn finish_invoke(
        &mut self,
        call: CallSiteValue<'ctx>,
        sig: &Signature,
        table: ir::ExceptionTable,
        normal: BasicBlock<'ctx>,
    ) -> Lowered<()> {
        self.builder.position_at_end(normal);
        let returned = self.returned(call, sig)?;
        self.returned_to(table, &returned, normal)
    }

    /// Goes on, from `from`, where a call with `table` returns to, with what it returned.
    fn returned_to(
        &mut self,
        table: ir::ExceptionTable,
        returned: &[BasicValueEnum<'ctx>],
        from: BasicBlock<'ctx>,
    ) -> Lowered<()> {
        let dfg = &self.func.dfg;
        let destination = dfg.exception_tables[table].normal_return();
        let target = destination.block(&dfg.value_lists);
        let phis = self.phis.get(&target).map_or(&[][..], Vec::as_slice);
        for (phi, arg) in std::iter::zip(phis, destination.args(&dfg.value_lists)) {
            let value = match arg {
                BlockArg::Value(value) => self.passed(*phi, value)?,
                BlockArg::TryCallRet(at) => self.as_phi(*phi, returned[at as usize])?,
                BlockArg::TryCallExn(_) => {
                    return Err(Unhandled::Shape("an exception where a call returns"));
                }
            };
            phi.add_incoming(&[(&value, from)]);
        }
        self.builder.build_unconditional_branch(self.blocks[&target])?;
        Ok(())
    }

    /// The block a jump goes to, having given its phis what the jump passes.
    fn edge(&self, call: BlockCall) -> Lowered<BasicBlock<'ctx>> {
        let from = self.builder.get_insert_block().expect("a block being filled");
        self.edge_from(call, from)
    }

    /// The same for one arm of a branch. An arm that passes values gets a block of its own,
    /// as two arms may reach the same block with different ones.
    fn branch_edge(&self, call: BlockCall) -> Lowered<BasicBlock<'ctx>> {
        if call.args(&self.func.dfg.value_lists).len() == 0 {
            return Ok(self.blocks[&call.block(&self.func.dfg.value_lists)]);
        }
        let from = self.builder.get_insert_block().expect("a block being filled");
        let arm = self.context.append_basic_block(self.function, "");
        self.builder.position_at_end(arm);
        let target = self.edge_from(call, arm)?;
        self.builder.build_unconditional_branch(target)?;
        self.builder.position_at_end(from);
        Ok(arm)
    }

    fn edge_from(&self, call: BlockCall, from: BasicBlock<'ctx>) -> Lowered<BasicBlock<'ctx>> {
        let dfg = &self.func.dfg;
        let target = call.block(&dfg.value_lists);
        let phis = self.phis.get(&target).map_or(&[][..], Vec::as_slice);
        for (phi, arg) in std::iter::zip(phis, call.args(&dfg.value_lists)) {
            let BlockArg::Value(value) = arg else {
                return Err(Unhandled::Shape("a call's result passed by a jump"));
            };
            phi.add_incoming(&[(&self.passed(*phi, value)?, from)]);
        }
        Ok(self.blocks[&target])
    }
}

impl From<BuilderError> for Unhandled {
    fn from(e: BuilderError) -> Self {
        Unhandled::Llvm(e.to_string())
    }
}
