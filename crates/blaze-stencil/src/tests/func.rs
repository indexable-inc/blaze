//! Whole functions compiled by both backends and called with the same arguments
//! (stencil.md, "Tests", "Per function, against Cranelift").
//!
//! Each *family* is a few functions shaped like the IR builder's (`hir-ty-codegen`'s
//! `mod.rs`: its signatures with `uext`/`sext`, `i128` pairs under
//! `enable_llvm_abi_extensions`, `StructArgument` copies, a `StructReturn` first parameter,
//! `try_call` with tag and cleanup pads, `br_table` for `match`, stack slots for places).
//! Function `i` is named `u0:i` and calls the others by those names. Every family is
//! compiled for every target the IR builder makes code for, by both backends, and the
//! stencil backend must refuse exactly what Cranelift refuses ([`compile_like`]).
//! On the host, [`world`] links each family once per *mix* (every choice of backend per
//! function, so each caller and callee pair is tried both ways), calls the same arguments
//! into each, and requires the results, the memory written and the traps to be what the
//! all-Cranelift link gives, bit for bit; where the interpreter can run a family, it is
//! the reference for that link too.
//!
//! What runs where: the tests that run code run on the host's own variant. The x86-64
//! ones (`StructArgument`, the System V `i128` that finds one register left, three
//! integer returns through `rcx`) run on x86-64 Linux, and the AArch64 ones (an `i128`
//! at an even register, eight returns of each class, Apple's packed stack arguments, and
//! value slots past `Glue::NEAR`: 32 KiB of 8-byte slots, 64 KiB of 16-byte ones) on
//! AArch64 Linux and macOS, each host's CI covering its own. x86-64 has no far slots (every
//! slot is a `disp32`), so there the far family is only a large frame. Windows' code is
//! compiled and held to Cranelift's acceptance here, never run.

use cranelift_codegen::{
    data_value::DataValue,
    ir::{self, InstBuilder, MemFlagsData, types},
    isa::{CallConv, OwnedTargetIsa},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{host, rng::Rng};
use crate::Target;

mod abi;
mod calls;
mod flow;
mod frames;
mod traps;
mod unwind;

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod world;

/// The user-name namespaces of `exec`'s linker: units, data objects, host functions.
pub const FUNCS: u32 = 0;
pub const HOST: u32 = 2;

/// Memory flags of every access the families make: aligned, in bounds, little-endian.
pub fn mem() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// What builds a family's functions for one target.
pub struct Mk {
    pub target: Target,
    pub isa: OwnedTargetIsa,
}

impl Mk {
    pub fn new(target: Target) -> Mk {
        Mk { target, isa: host::isa(&target) }
    }

    pub fn cc(&self) -> CallConv {
        host::call_conv(&self.target)
    }

    pub fn x64(&self) -> bool {
        self.target.arch == blaze_codegen::Arch::X86_64
    }

    pub fn windows(&self) -> bool {
        self.target.os == crate::Os::Windows
    }

    /// Whether the convention extends a narrow `uext`/`sext` value to 64 bits in its
    /// register: x86-64 always, Apple's AArch64 in registers, AAPCS64 never
    /// (`get_ext_mode` of each `abi.rs`).
    pub fn extends(&self) -> bool {
        self.x64() || self.target.os == crate::Os::MacOs
    }

    pub fn sig(&self, params: &[ir::AbiParam], rets: &[ir::AbiParam]) -> ir::Signature {
        self.sig_in(self.cc(), params, rets)
    }

    pub fn sig_in(&self, cc: CallConv, params: &[ir::AbiParam], rets: &[ir::AbiParam]) -> ir::Signature {
        let mut sig = ir::Signature::new(cc);
        sig.params.extend_from_slice(params);
        sig.returns.extend_from_slice(rets);
        sig
    }

    pub fn sig_of(&self, params: &[ir::Type], rets: &[ir::Type]) -> ir::Signature {
        let params: Vec<ir::AbiParam> = params.iter().map(|ty| ir::AbiParam::new(*ty)).collect();
        let rets: Vec<ir::AbiParam> = rets.iter().map(|ty| ir::AbiParam::new(*ty)).collect();
        self.sig(&params, &rets)
    }

    /// Function `u0:index` of `sig`: `body` gets the builder in the entry block and the
    /// entry's parameters, and ends every block it makes.
    pub fn func(
        &self,
        index: u32,
        sig: ir::Signature,
        body: impl FnOnce(&mut FunctionBuilder<'_>, &[ir::Value]),
    ) -> ir::Function {
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(FUNCS, index), sig);
        let mut cx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut func, &mut cx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        body(&mut b, &params);
        b.seal_all_blocks();
        b.finalize(self.isa.frontend_config());
        func
    }
}

/// Imports `namespace:index` with `sig`.
pub fn import(
    b: &mut FunctionBuilder<'_>,
    namespace: u32,
    index: u32,
    sig: ir::Signature,
    colocated: bool,
) -> ir::FuncRef {
    let signature = b.import_signature(sig);
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(namespace, index));
    b.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(name),
        signature,
        colocated,
        patchable: false,
    })
}

/// Calls `func` and gives its results.
pub fn call(b: &mut FunctionBuilder<'_>, func: ir::FuncRef, args: &[ir::Value]) -> Vec<ir::Value> {
    let call = b.ins().call(func, args);
    b.inst_results(call).to_vec()
}

pub fn call_indirect(
    b: &mut FunctionBuilder<'_>,
    sig: ir::SigRef,
    callee: ir::Value,
    args: &[ir::Value],
) -> Vec<ir::Value> {
    let call = b.ins().call_indirect(sig, callee, args);
    b.inst_results(call).to_vec()
}

/// Block arguments of `values`.
pub fn args(values: &[ir::Value]) -> Vec<ir::BlockArg> {
    values.iter().map(|value| ir::BlockArg::Value(*value)).collect()
}

/// An `i128` of `bits`, as the IR builder makes one: an `i64` constant extended.
pub fn i128_const(b: &mut FunctionBuilder<'_>, bits: i64) -> ir::Value {
    let low = b.ins().iconst(types::I64, bits);
    b.ins().sextend(types::I128, low)
}

/// Mixes `x` into `acc`, so each operand's position changes the result: `rotl(acc, 7) ^ x`
/// then `+ k`, in `acc`'s type (an integer of any width).
pub fn mix(b: &mut FunctionBuilder<'_>, acc: ir::Value, x: ir::Value, k: i64) -> ir::Value {
    let ty = b.func.dfg.value_type(acc);
    let seven = b.ins().iconst(types::I64, 7);
    let rotated = b.ins().rotl(acc, seven);
    let mixed = b.ins().bxor(rotated, x);
    let k = if ty == types::I128 { i128_const(b, k) } else { b.ins().iconst(ty, k) };
    b.ins().iadd(mixed, k)
}

/// `x` widened (or kept) to `i64`, signed or not.
pub fn widen(b: &mut FunctionBuilder<'_>, x: ir::Value, signed: bool) -> ir::Value {
    let ty = b.func.dfg.value_type(x);
    match ty.bits() {
        64 => x,
        bits if bits > 64 => b.ins().ireduce(types::I64, x),
        _ if signed => b.ins().sextend(types::I64, x),
        _ => b.ins().uextend(types::I64, x),
    }
}

/// A stack slot of `size` bytes aligned to `1 << align_shift`.
pub fn slot(b: &mut FunctionBuilder<'_>, size: u32, align_shift: u8) -> ir::StackSlot {
    b.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, size, align_shift))
}

/// Functions compiled and called together: `funcs[i]` is `u0:i`, `funcs[0]` is called.
pub struct Family {
    pub name: &'static str,
    pub funcs: Vec<ir::Function>,
    /// Whether Cranelift refuses the family on the target it was made for, where the
    /// family exists to show a limit; `None` where it need not be either.
    pub refused: Option<bool>,
}

impl Family {
    pub fn new(name: &'static str, funcs: Vec<ir::Function>) -> Family {
        Family { name, funcs, refused: Some(false) }
    }

    pub fn refused(mut self, refused: bool) -> Family {
        self.refused = Some(refused);
        self
    }

    pub fn either(mut self) -> Family {
        self.refused = None;
        self
    }

    /// Every function's CLIF, for a failure message.
    pub fn clif(&self) -> String {
        let mut out = String::new();
        for func in &self.funcs {
            out.push_str(&func.display().to_string());
            out.push('\n');
        }
        out
    }
}

/// Every family of the file, for any target.
pub fn families() -> Vec<fn(&Mk) -> Family> {
    let mut out = Vec::new();
    out.extend_from_slice(calls::FAMILIES);
    out.extend_from_slice(abi::FAMILIES);
    out.extend_from_slice(flow::FAMILIES);
    out.extend_from_slice(frames::FAMILIES);
    out.extend_from_slice(unwind::FAMILIES);
    out.extend_from_slice(traps::FAMILIES);
    out
}

/// A random value of `ty`, biased a third of the time to an edge of an integer type.
pub fn random(rng: &mut Rng, ty: ir::Type) -> DataValue {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    if ty.is_int() && rng.below(3) == 0 {
        let width = ty.bytes() as usize;
        let top = 1u128 << (ty.bits() - 1);
        let edge = match rng.below(6) {
            0 => 0,
            1 => 1,
            2 => u128::MAX,
            3 => top,
            4 => top - 1,
            _ => 2,
        };
        bytes[..width].copy_from_slice(&edge.to_le_bytes()[..width]);
    }
    DataValue::read_from_slice_le(&bytes, ty)
}

/// Each family of `families`, on `target`, compiles with the stencil backend exactly when it
/// compiles with Cranelift (a panic in Cranelift's lowering, such as its refusal of
/// `StructArgument` on AArch64, counts as a refusal), and Cranelift refuses what the family
/// says it does.
fn compile_like(families: &[fn(&Mk) -> Family], target: Target) {
    let mut failures = Vec::new();
    let mk = Mk::new(target);
    for make in families {
        let family = make(&mk);
        let mut accepted = true;
        for (i, func) in family.funcs.iter().enumerate() {
            let cranelift = host::cranelift_caught(func, &*mk.isa);
            let stencil = crate::compile(func, &target);
            accepted &= cranelift.is_ok();
            match (&cranelift, &stencil) {
                (Ok(_), Err(e)) => failures.push(format!(
                    "{} on {}: u0:{i} compiles with Cranelift, the stencil backend refuses it: {e}\n{}",
                    family.name,
                    host::triple(&target),
                    func.display()
                )),
                (Err(e), Ok(_)) => failures.push(format!(
                    "{} on {}: u0:{i} is refused by Cranelift ({e}), the stencil backend compiles it\n{}",
                    family.name,
                    host::triple(&target),
                    func.display()
                )),
                _ => {}
            }
        }
        if let Some(refused) = family.refused
            && refused == accepted
        {
            failures.push(format!(
                "{} on {}: Cranelift {} it, the family says it {}",
                family.name,
                host::triple(&target),
                if accepted { "compiles" } else { "refuses" },
                if refused { "is refused" } else { "compiles" },
            ));
        }
    }
    assert!(failures.is_empty(), "{} disagreements:\n{}", failures.len(), failures.join("\n"));
}

/// [`compile_like`] split by family module and target, so that each test fits a debug run.
macro_rules! compile_like_cranelift {
    ($($name:ident: $families:path, $target:literal;)*) => {
        $(
            #[test]
            fn $name() {
                compile_like($families, host::TARGETS[$target]);
            }
        )*
    };
}

compile_like_cranelift! {
    compile_like_cranelift_calls_x86_64_linux: calls::FAMILIES, 0;
    compile_like_cranelift_calls_aarch64_linux: calls::FAMILIES, 1;
    compile_like_cranelift_calls_aarch64_macos: calls::FAMILIES, 2;
    compile_like_cranelift_calls_x86_64_windows: calls::FAMILIES, 3;
    compile_like_cranelift_abi_x86_64_linux: abi::FAMILIES, 0;
    compile_like_cranelift_abi_aarch64_linux: abi::FAMILIES, 1;
    compile_like_cranelift_abi_aarch64_macos: abi::FAMILIES, 2;
    compile_like_cranelift_abi_x86_64_windows: abi::FAMILIES, 3;
    compile_like_cranelift_flow_x86_64_linux: flow::FAMILIES, 0;
    compile_like_cranelift_flow_aarch64_linux: flow::FAMILIES, 1;
    compile_like_cranelift_flow_aarch64_macos: flow::FAMILIES, 2;
    compile_like_cranelift_flow_x86_64_windows: flow::FAMILIES, 3;
    compile_like_cranelift_frames_x86_64_linux: frames::FAMILIES, 0;
    compile_like_cranelift_frames_aarch64_linux: frames::FAMILIES, 1;
    compile_like_cranelift_frames_aarch64_macos: frames::FAMILIES, 2;
    compile_like_cranelift_frames_x86_64_windows: frames::FAMILIES, 3;
    compile_like_cranelift_unwind_x86_64_linux: unwind::FAMILIES, 0;
    compile_like_cranelift_unwind_aarch64_linux: unwind::FAMILIES, 1;
    compile_like_cranelift_unwind_aarch64_macos: unwind::FAMILIES, 2;
    compile_like_cranelift_unwind_x86_64_windows: unwind::FAMILIES, 3;
    compile_like_cranelift_traps_x86_64_linux: traps::FAMILIES, 0;
    compile_like_cranelift_traps_aarch64_linux: traps::FAMILIES, 1;
    compile_like_cranelift_traps_aarch64_macos: traps::FAMILIES, 2;
    compile_like_cranelift_traps_x86_64_windows: traps::FAMILIES, 3;
}

#[test]
fn families_have_distinct_names_and_name_their_functions_in_order() {
    let mk = Mk::new(host::TARGETS[0]);
    let mut names = Vec::new();
    for make in families() {
        let family = make(&mk);
        assert!(!family.funcs.is_empty(), "{}", family.name);
        for (i, func) in family.funcs.iter().enumerate() {
            assert_eq!(func.name, ir::UserFuncName::user(FUNCS, i as u32), "{}", family.name);
        }
        names.push(family.name);
    }
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "two families of one name");
}
