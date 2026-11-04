//! A function over 1 MiB of code: on AArch64 its `b.cond`s to blocks, trap stubs and back
//! edges and its `adr`s to constants reach only ±1 MiB, so the code needs islands
//! ([`crate::buf`]) of veneers and placed constants; the results must be Cranelift's.

use cranelift_codegen::ir::{self, InstBuilder, condcodes::IntCC, types};

use super::{
    Family, Mk,
    world::{Opts, check, host_mk, i64s},
};

const I64: ir::Type = types::I64;
/// Rounds of the body: enough for well over 1 MiB of AArch64 stencil code.
const ROUNDS: i64 = 24_000;

/// `(n, x, d) -> ...`: a constant from the pool, a division by `d` (a trap stub at the end),
/// a branch over the body when `x` is zero, and a body of `ROUNDS` multiply-adds through a
/// stack slot run `n` times, its back edge a branch over all of it.
fn long(mk: &Mk) -> ir::Function {
    mk.func(0, mk.sig_of(&[I64, I64, I64], &[I64]), |b, p| {
        let (n, x, d) = (p[0], p[1], p[2]);
        let acc = b.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        let left = b.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        let body = b.create_block();
        let exit = b.create_block();
        let pool = b.func.dfg.constants.insert(ir::ConstantData::from(
            [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 1, 2, 3, 4, 5, 6, 7, 8].as_slice(),
        ));
        let vector = b.ins().vconst(types::I64X2, pool);
        let lane = b.ins().extractlane(vector, 1);
        let quotient = b.ins().udiv(x, d);
        let start = b.ins().iadd(quotient, lane);
        b.ins().stack_store(I64, start, acc, 0);
        b.ins().stack_store(I64, n, left, 0);
        b.ins().brif(x, body, &[], exit, &[]);

        b.switch_to_block(body);
        for round in 0..ROUNDS {
            let it = b.ins().stack_load(I64, I64, acc, 0);
            let it = b.ins().imul_imm_s(it, 3);
            let it = b.ins().iadd_imm_s(it, round);
            b.ins().stack_store(I64, it, acc, 0);
        }
        let more = b.ins().stack_load(I64, I64, left, 0);
        let more = b.ins().iadd_imm_s(more, -1);
        b.ins().stack_store(I64, more, left, 0);
        let again = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, more, 0);
        b.ins().brif(again, body, &[], exit, &[]);

        b.switch_to_block(exit);
        let out = b.ins().stack_load(I64, I64, acc, 0);
        b.ins().return_(&[out]);
    })
}

#[test]
fn a_function_past_every_branch_reach_runs_as_cranelifts() {
    let Some(host) = host_mk() else { return };
    for counts in [true, false] {
        let mk = Mk::new(crate::Target { counts, ..host.target });
        let func = long(&mk);
        let code =
            crate::compile(&func, &mk.target).unwrap_or_else(|e| panic!("counts {counts}: {e}"));
        if !mk.x64() {
            assert!(
                code.bytes.len() > 9 << 17,
                "counts {counts}: {} bytes reach no limit",
                code.bytes.len()
            );
        }
        let again = crate::compile(&func, &mk.target).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(code.bytes, again.bytes, "counts {counts}: two compiles differ");
        let family = Family::new("long", vec![func]);
        let inputs = [i64s(&[1, 7, 3]), i64s(&[3, -5, 2]), i64s(&[2, 0, 9])];
        check(&mk, &family, &inputs, &Opts::plain());
    }
}
