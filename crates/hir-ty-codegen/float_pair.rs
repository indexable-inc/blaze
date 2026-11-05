//! Two floats returned by a convention Cranelift returns one float in.
//!
//! Cranelift's `windows_fastcall` returns one float, in `xmm0`. A function that returns two
//! (a scalar pair, `Pos2` or a `PhysicalSize<f64>`) returns them as two integers of the same
//! widths, in `rax` and `rdx`, and a call of one bitcasts what it gets back. rustc returns
//! the pair in `xmm0` and `xmm1`, which only a program that links rustc's objects would need.

#![forbid(unsafe_code)]

use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{self, InstBuilder, MemFlagsData, Opcode, Signature, types},
    isa::CallConv,
};

use super::{Result, fragment::Code};

/// What [`lower`] changed of a function, to check and put back in its code.
pub(super) struct Lowered {
    /// The signature the callers of the function know.
    signature: Signature,
}

/// The types of the pair `signature` returns, if Cranelift cannot return it.
fn pair(signature: &Signature) -> Option<[ir::Type; 2]> {
    let [a, b] = *signature.returns.as_slice() else { return None };
    let limited = signature.call_conv == CallConv::WindowsFastcall;
    (limited && a.value_type.is_float() && b.value_type.is_float())
        .then_some([a.value_type, b.value_type])
}

/// The integer type that holds as many bits as the float `ty`.
fn bits_of(ty: ir::Type) -> ir::Type {
    ty.as_int()
}

/// Has `func` return the pair of floats it returns, and be returned the pairs it calls return,
/// as two integers of the same widths, which `windows_fastcall` returns in `rax` and `rdx`.
/// `None` when it has neither, or is returned a pair by what is not a plain call, which is
/// left for Cranelift to refuse.
///
/// This is not where rustc returns a pair of floats (`xmm0` and `xmm1`), which a program that
/// links rustc's objects needs; a build with nothing of rustc's has no code that tells.
pub(super) fn lower(func: &mut ir::Function) -> Option<Lowered> {
    let returned = pair(&func.signature);
    if returned.is_none() && !func.dfg.signatures.iter().any(|(_, it)| pair(it).is_some()) {
        return None;
    }
    let pairs: Vec<(ir::SigRef, [ir::Type; 2])> =
        // alloc: only a `windows_fastcall` function that returns or calls a pair of floats
        func.dfg.signatures.iter().filter_map(|(id, it)| Some((id, pair(it)?))).collect();
    let called = |func: &ir::Function, inst| {
        let signature = func.dfg.call_signature(inst)?;
        pairs.iter().find(|(id, _)| *id == signature).map(|&(_, types)| types)
    };
    let insts: Vec<ir::Inst> =
        // alloc: only such a function; the walk below changes the layout it lists
        func.layout.blocks().flat_map(|block| func.layout.block_insts(block)).collect();
    let plain = |opcode| matches!(opcode, Opcode::Call | Opcode::CallIndirect);
    if insts.iter().any(|&it| called(func, it).is_some() && !plain(func.dfg.insts[it].opcode())) {
        return None;
    }
    // clone: only such a function; its callers' signature outlives the one changed here
    let lowered = Lowered { signature: func.signature.clone() };
    let integers =
        |[a, b]: [ir::Type; 2]| [ir::AbiParam::new(bits_of(a)), ir::AbiParam::new(bits_of(b))];
    for &(signature, types) in &pairs {
        // alloc: only such a function; Cranelift's signature owns its returns
        func.dfg.signatures[signature].returns = integers(types).to_vec();
    }
    if let Some(types) = returned {
        // alloc: only such a function; Cranelift's signature owns its returns
        func.signature.returns = integers(types).to_vec();
    }
    for inst in insts {
        if let Some([a, b]) = called(func, inst) {
            let &[first, second] = func.dfg.inst_results(inst) else {
                unreachable!("the signature has two results")
            };
            func.dfg.clear_results(inst);
            func.dfg.make_inst_results(inst, types::INVALID);
            let &[low, high] = func.dfg.inst_results(inst) else {
                unreachable!("the signature has two results")
            };
            let mut cursor = FuncCursor::new(func).after_inst(inst);
            for (was, bits, ty) in [(first, low, a), (second, high, b)] {
                let float = cursor.ins().bitcast(ty, MemFlagsData::new(), bits);
                cursor.func.dfg.change_to_alias(was, float);
            }
        } else if returned.is_some() && func.dfg.insts[inst].opcode() == Opcode::Return {
            let &[first, second] = func.dfg.inst_args(inst) else {
                unreachable!("the signature has two results")
            };
            let Some([a, b]) = returned else { continue };
            let mut cursor = FuncCursor::new(func).at_inst(inst);
            let first = cursor.ins().bitcast(bits_of(a), MemFlagsData::new(), first);
            let second = cursor.ins().bitcast(bits_of(b), MemFlagsData::new(), second);
            cursor.ins().return_(&[first, second]);
            cursor.remove_inst();
        }
    }
    Some(lowered)
}

impl Lowered {
    /// Gives `code` the signature its callers know.
    pub(super) fn hold(self, _: &cranelift_codegen::Context, code: &mut Code) -> Result<()> {
        code.signature = self.signature;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::{
        ir::{AbiParam, ExtFuncData, ExternalName, Function, UserFuncName},
        settings::{self, Configurable},
        verify_function,
    };
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    use super::*;

    /// A function `(f32) -> (f32, f32)` that calls one of its own signature and returns the pair
    /// swapped: both ends of the pair become two integers, what the call returns is bitcast back,
    /// and the function still verifies.
    #[test]
    fn a_pair_of_floats_is_returned_and_read_as_two_integers_of_the_same_widths() {
        let mut signature = Signature::new(CallConv::WindowsFastcall);
        signature.params.push(AbiParam::new(types::F32));
        signature.returns.extend([AbiParam::new(types::F32), AbiParam::new(types::F32)]);
        let mut func = Function::with_name_signature(UserFuncName::default(), signature.clone());
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let [arg] = *builder.block_params(block) else { unreachable!() };
        let imported = builder.import_signature(signature);
        let callee = builder.import_function(ExtFuncData {
            name: ExternalName::user(ir::UserExternalNameRef::from_u32(0)),
            signature: imported,
            colocated: true,
            patchable: false,
        });
        let call = builder.ins().call(callee, &[arg]);
        let [first, second] = *builder.inst_results(call) else { unreachable!() };
        builder.ins().return_(&[second, first]);
        builder.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup(target_lexicon::triple!("x86_64-pc-windows-msvc"))
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        builder.finalize(isa.frontend_config());

        let lowered = lower(&mut func).expect("a function that returns and calls a pair");
        let integers = [AbiParam::new(types::I32), AbiParam::new(types::I32)];
        assert_eq!(func.signature.returns, integers);
        assert_eq!(func.dfg.signatures.iter().next().unwrap().1.returns, integers);
        let mut flags = settings::builder();
        flags.enable("enable_verifier").unwrap();
        verify_function(&func, &settings::Flags::new(flags)).unwrap();
        // What its callers know of it is what it was.
        let floats = [AbiParam::new(types::F32), AbiParam::new(types::F32)];
        assert_eq!(lowered.signature.returns, floats);
        // Another convention is left alone.
        let mut elsewhere = Function::new();
        elsewhere.signature = Signature::new(CallConv::SystemV);
        elsewhere.signature.returns.extend(floats);
        assert!(lower(&mut elsewhere).is_none());
    }
}
