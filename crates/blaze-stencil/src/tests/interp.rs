//! `cranelift-interpreter` as the reference for what IR means, and the one rule the tests
//! compare results by.
//!
//! Against Cranelift's own machine code, stencil code must agree bit for bit, NaN payloads
//! included (stencil.md, "Semantics, op by op": the same hardware instructions). Against the
//! interpreter, which computes with the host's Rust float operations, a NaN only has to be a
//! NaN: Cranelift's semantics leave NaN bits nondeterministic, which is why its own
//! `cranelift-fuzzgen` canonicalizes NaNs before comparing with the interpreter.
//!
//! The interpreter runs everywhere; it needs no code mapped.

use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    data_value::DataValue,
    ir::{self, InstBuilder, types},
};
use cranelift_interpreter::{
    environment::FunctionStore,
    interpreter::{Interpreter, InterpreterState},
    state::State,
    step::{ControlFlow, CraneliftTrap},
};

/// Instructions the interpreter may step before a run is called endless.
const FUEL: u64 = 10_000_000;

/// How a run of the reference ended.
#[derive(Clone, PartialEq, Debug)]
pub enum Run {
    Returned(Vec<DataValue>),
    /// A trap with its code, or `None` for one with no code (`unreachable`, a bad signature).
    Trapped(Option<ir::TrapCode>),
}

/// Runs `funcs[entry]` on `args` in the interpreter. Each function is found by its name
/// (`func.name`, which is how a `call` in another one names it: `u0:3` or `%name`). An error
/// is what the interpreter can't do (an operation it lacks, its fuel run out).
pub fn run(funcs: &[&ir::Function], entry: usize, args: &[DataValue]) -> Result<Run, String> {
    let funcs: Vec<ir::Function> = funcs.iter().map(|func| interpretable(func)).collect();
    // The interpreter's address of the first frame's first stack slot is 0, which code
    // testing a pointer for null takes for one: a frame of 16 bytes below the entry's.
    let mut base = ir::Function::new();
    base.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, 16, 4));
    let mut store = FunctionStore::default();
    for func in &funcs {
        store.add(func.name.to_string(), func);
    }
    let mut state = InterpreterState::default().with_function_store(store);
    state.push_frame(&base);
    let mut interpreter = Interpreter::new(state).with_fuel(Some(FUEL));
    match interpreter.call_by_name(&funcs[entry].name.to_string(), args) {
        Ok(ControlFlow::Return(values)) => Ok(Run::Returned(values.to_vec())),
        Ok(ControlFlow::Trap(CraneliftTrap::User(code))) => Ok(Run::Trapped(Some(code))),
        Ok(ControlFlow::Trap(other)) => match format!("{other:?}") {
            it if it.contains("Unreachable") => Ok(Run::Trapped(None)),
            it => Err(format!("the interpreter stopped with {it}")),
        },
        Ok(_) => Err("the interpreter ended the call with no return".to_owned()),
        Err(e) => Err(format!("the interpreter: {e}")),
    }
}

/// `func` with what the interpreter gets wrong written as what it gets right:
///
/// - each `avg_round` as `(x | y) - ((x ^ y) >> 1)`, the same lane by lane: the interpreter's
///   adds its operands as one scalar, which panics on a vector and wraps before it halves on
///   a scalar;
/// - each `srem` by -1 as one by 1, both 0: the interpreter's traps on the minimum by -1,
///   which only a zero divisor does;
/// - each `bitcast` from or to `i128` or `f128` as a store to a 16-byte stack slot and a load
///   of the other type, the same bytes little-endian (a bitcast's only order the interpreter
///   takes): the interpreter's splits the value into lanes, which panics on a 128-bit lane, and
///   makes an `i128` one `i8` lane of a vector;
/// - each extending vector load (`sload8x8` to `uload32x2`), which the interpreter does not
///   have, as the `i64` load it is, its bytes made the narrow lanes and those widened.
fn interpretable(func: &ir::Function) -> ir::Function {
    let mut func = func.clone();
    let mut cursor = FuncCursor::new(&mut func);
    while cursor.next_block().is_some() {
        while let Some(inst) = cursor.next_inst() {
            match cursor.func.dfg.insts[inst].opcode() {
                ir::Opcode::Bitcast => {
                    let &[x] = cursor.func.dfg.inst_args(inst) else { unreachable!("bitcast takes one operand") };
                    let from = cursor.func.dfg.value_type(x);
                    let to = cursor.func.dfg.value_type(cursor.func.dfg.first_result(inst));
                    if from.lane_bits() <= 64 && to.lane_bits() <= 64 {
                        continue;
                    }
                    let slot = cursor.func.create_sized_stack_slot(ir::StackSlotData::new(
                        ir::StackSlotKind::ExplicitSlot,
                        16,
                        4,
                    ));
                    cursor.ins().stack_store(types::I64, x, slot, 0);
                    cursor.func.replace(inst).stack_load(types::I64, to, slot, 0);
                }
                opcode @ (ir::Opcode::Sload8x8
                | ir::Opcode::Uload8x8
                | ir::Opcode::Sload16x4
                | ir::Opcode::Uload16x4
                | ir::Opcode::Sload32x2
                | ir::Opcode::Uload32x2) => {
                    let ir::InstructionData::Load { arg, offset, .. } = cursor.func.dfg.insts[inst] else {
                        unreachable!("an extending load is a load")
                    };
                    let flags = cursor.func.dfg.insts[inst]
                        .memflags_data(&cursor.func.dfg)
                        .expect("a load has memory flags");
                    let narrow = match opcode {
                        ir::Opcode::Sload8x8 | ir::Opcode::Uload8x8 => types::I8X16,
                        ir::Opcode::Sload16x4 | ir::Opcode::Uload16x4 => types::I16X8,
                        _ => types::I32X4,
                    };
                    let word = cursor.ins().load(types::I64, flags, arg, offset);
                    let wide = cursor.ins().scalar_to_vector(types::I64X2, word);
                    let little = ir::MemFlagsData::new().with_endianness(ir::Endianness::Little);
                    let lanes = cursor.ins().bitcast(narrow, little, wide);
                    if matches!(opcode, ir::Opcode::Sload8x8 | ir::Opcode::Sload16x4 | ir::Opcode::Sload32x2) {
                        cursor.func.replace(inst).swiden_low(lanes);
                    } else {
                        cursor.func.replace(inst).uwiden_low(lanes);
                    }
                }
                ir::Opcode::AvgRound => {
                    let &[x, y] = cursor.func.dfg.inst_args(inst) else {
                        unreachable!("avg_round takes two operands")
                    };
                    let or = cursor.ins().bor(x, y);
                    let xor = cursor.ins().bxor(x, y);
                    let one = cursor.ins().iconst(types::I32, 1);
                    let half = cursor.ins().ushr(xor, one);
                    cursor.func.replace(inst).isub(or, half);
                }
                ir::Opcode::Srem => {
                    let &[x, y] = cursor.func.dfg.inst_args(inst) else { unreachable!("srem takes two operands") };
                    let ty = cursor.func.dfg.value_type(y);
                    let all_ones = int(&mut cursor, ty, -1);
                    let minus_one = cursor.ins().icmp(ir::condcodes::IntCC::Equal, y, all_ones);
                    let one = int(&mut cursor, ty, 1);
                    let divisor = cursor.ins().select(minus_one, one, y);
                    cursor.func.replace(inst).srem(x, divisor);
                }
                _ => {}
            }
        }
    }
    func
}

/// `value` as a constant of the integer type `ty`, which `iconst` makes only up to 64 bits.
fn int(cursor: &mut FuncCursor<'_>, ty: ir::Type, value: i64) -> ir::Value {
    if ty == types::I128 {
        let wide = cursor.ins().iconst(types::I64, value);
        return cursor.ins().sextend(types::I128, wide);
    }
    let bits = ty.bits();
    let value = if bits >= 64 { value } else { value & ((1i64 << bits) - 1) };
    cursor.ins().iconst(ty, value)
}

/// How a call of machine code ended (`exec`'s calls in a forked child).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome<T> {
    Returned(T),
    /// The child ended by this signal: a trap (`SIGILL` for `ud2`/`udf`, `SIGFPE` for x86's
    /// divide, `SIGSEGV`/`SIGBUS` for a bad access, `SIGTRAP` for `brk`/`int3`).
    Trapped(i32),
}

impl<T> Outcome<T> {
    pub fn trapped(&self) -> bool {
        matches!(self, Outcome::Trapped(_))
    }
}

/// How strictly NaNs compare.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nans {
    /// Bit for bit: stencil code against Cranelift's.
    Exact,
    /// Any NaN equals any NaN, lane by lane: either against the interpreter.
    Any,
}

/// Whether `a` and `b`, values of `ty`, are the same result.
pub fn same(ty: ir::Type, a: &DataValue, b: &DataValue, nans: Nans) -> bool {
    let (a, b) = (bytes(a), bytes(b));
    let width = ty.bytes() as usize;
    if a[..width] == b[..width] {
        return true;
    }
    if nans == Nans::Exact {
        return false;
    }
    let lane = ty.lane_type();
    if !lane.is_float() {
        return false;
    }
    let lane_bytes = lane.bytes() as usize;
    a[..width].chunks_exact(lane_bytes).zip(b[..width].chunks_exact(lane_bytes)).all(|(a, b)| {
        a == b || (is_nan(lane, a) && is_nan(lane, b))
    })
}

/// Whether every value of `a` and `b` (of `tys`) is the same result.
pub fn same_all(tys: &[ir::Type], a: &[DataValue], b: &[DataValue], nans: Nans) -> bool {
    a.len() == tys.len()
        && b.len() == tys.len()
        && tys.iter().zip(a.iter().zip(b)).all(|(ty, (a, b))| same(*ty, a, b, nans))
}

/// Whether `got` (of a backend's code) is what `reference` (the interpreter's run) says, for
/// a function returning `tys`: equal values with any NaN for a NaN, or a trap for a trap.
/// Trap codes are not compared: stencil code reports a trap by its signal only.
pub fn agrees(tys: &[ir::Type], reference: &Run, got: &Outcome<Vec<DataValue>>) -> Result<(), String> {
    match (reference, got) {
        (Run::Returned(want), Outcome::Returned(got)) => {
            if same_all(tys, want, got, Nans::Any) {
                Ok(())
            } else {
                Err(format!("the interpreter returns {want:?}, the code {got:?}"))
            }
        }
        (Run::Trapped(_), Outcome::Trapped(_)) => Ok(()),
        (Run::Returned(want), Outcome::Trapped(signal)) => {
            Err(format!("the interpreter returns {want:?}, the code traps (signal {signal})"))
        }
        (Run::Trapped(code), Outcome::Returned(got)) => {
            Err(format!("the interpreter traps ({code:?}), the code returns {got:?}"))
        }
    }
}

/// The 16 little-endian bytes of `value`, zero past its width.
pub fn bytes(value: &DataValue) -> [u8; 16] {
    let mut out = [0u8; 16];
    value.write_to_slice_le(&mut out);
    out
}

fn is_nan(lane: ir::Type, bytes: &[u8]) -> bool {
    let mut wide = [0u8; 16];
    wide[..bytes.len()].copy_from_slice(bytes);
    let bits = u128::from_le_bytes(wide);
    let (exp, man) = match lane.bits() {
        16 => (0x1f << 10, (1 << 10) - 1),
        32 => (0xff << 23, (1 << 23) - 1),
        64 => (0x7ff << 52, (1 << 52) - 1),
        128 => (0x7fff << 112, (1 << 112) - 1),
        _ => return false,
    };
    bits & exp == exp && bits & man != 0
}

#[test]
fn nans_compare_by_rule() {
    use cranelift_codegen::ir::immediates::Ieee32;
    let quiet = DataValue::F32(Ieee32::with_bits(0x7fc0_0000));
    let payload = DataValue::F32(Ieee32::with_bits(0x7fc1_2345));
    let one = DataValue::F32(Ieee32::with_bits(0x3f80_0000));
    assert!(same(types::F32, &quiet, &payload, Nans::Any));
    assert!(!same(types::F32, &quiet, &payload, Nans::Exact));
    assert!(!same(types::F32, &quiet, &one, Nans::Any));
    let mut lanes = [0u8; 16];
    lanes[..4].copy_from_slice(&0x7fc0_0000u32.to_le_bytes());
    let mut other = lanes;
    other[..4].copy_from_slice(&0xffc0_0001u32.to_le_bytes());
    assert!(same(types::F32X4, &DataValue::V128(lanes), &DataValue::V128(other), Nans::Any));
    assert!(!same(types::I32X4, &DataValue::V128(lanes), &DataValue::V128(other), Nans::Any));
}

#[test]
fn the_interpreter_runs_an_add() {
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.push(ir::AbiParam::new(types::I32));
    sig.returns.push(ir::AbiParam::new(types::I32));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let x = b.block_params(entry)[0];
    let y = b.ins().iadd_imm(x, 1);
    b.ins().return_(&[y]);
    b.seal_all_blocks();
    let config = super::host::isa(&super::host::TARGETS[0]).frontend_config();
    b.finalize(config);
    assert_eq!(
        run(&[&func], 0, &[DataValue::I32(i32::MAX)]),
        Ok(Run::Returned(vec![DataValue::I32(i32::MIN)]))
    );
}

/// A bitcast between a 128-bit scalar and a vector keeps the bytes, little-endian.
#[test]
fn the_interpreter_runs_a_bitcast_of_a_128_bit_lane() {
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let bitcast = |from: ir::Type, to: ir::Type, arg: DataValue| {
        let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
        sig.params.push(ir::AbiParam::new(from));
        sig.returns.push(ir::AbiParam::new(to));
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
        let mut cx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut func, &mut cx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let x = b.block_params(entry)[0];
        let y = b.ins().bitcast(to, ir::MemFlagsData::new().with_endianness(ir::Endianness::Little), x);
        b.ins().return_(&[y]);
        b.seal_all_blocks();
        let config = super::host::isa(&super::host::TARGETS[0]).frontend_config();
        b.finalize(config);
        run(&[&func], 0, &[arg])
    };
    let bytes: [u8; 16] = std::array::from_fn(|i| i as u8 * 16 + 1);
    let wide = u128::from_le_bytes(bytes);
    assert_eq!(
        bitcast(types::I128, types::I8X16, DataValue::I128(wide as i128)),
        Ok(Run::Returned(vec![DataValue::V128(bytes)]))
    );
    assert_eq!(
        bitcast(types::I8X16, types::I128, DataValue::V128(bytes)),
        Ok(Run::Returned(vec![DataValue::I128(wide as i128)]))
    );
    let f128 = DataValue::F128(cranelift_codegen::ir::immediates::Ieee128::with_bits(wide));
    assert_eq!(bitcast(types::F128, types::I32X4, f128), Ok(Run::Returned(vec![DataValue::V128(bytes)])));
}

/// `avg_round` is `(x + y + 1) >> 1` of each lane, computed wide.
#[test]
fn the_interpreter_runs_a_vector_avg_round() {
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.extend([ir::AbiParam::new(types::I8X16), ir::AbiParam::new(types::I8X16)]);
    sig.returns.push(ir::AbiParam::new(types::I8X16));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let avg = b.ins().avg_round(x, y);
    b.ins().return_(&[avg]);
    b.seal_all_blocks();
    let config = super::host::isa(&super::host::TARGETS[0]).frontend_config();
    b.finalize(config);
    let x: [u8; 16] = std::array::from_fn(|i| [0, 0, 1, 254, 255, 255, 7, 128][i % 8]);
    let y: [u8; 16] = std::array::from_fn(|i| [0, 1, 1, 255, 254, 255, 100, 127][i % 8]);
    let want: [u8; 16] = std::array::from_fn(|i| ((u16::from(x[i]) + u16::from(y[i]) + 1) >> 1) as u8);
    assert_eq!(
        run(&[&func], 0, &[DataValue::V128(x), DataValue::V128(y)]),
        Ok(Run::Returned(vec![DataValue::V128(want)]))
    );
}
