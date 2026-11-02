//! Traps in whole functions, called in a forked child (`values_forked`) and compared as
//! traps: a divide deep in a loop, every other iteration in a callee of the other backend,
//! trapping on zero and on `MIN / -1` at the iteration the arguments ask for; and
//! `trapnz`/`trapz` on conditions of each width before a result.

use cranelift_codegen::ir::{self, InstBuilder, condcodes::IntCC, types};

use super::{FUNCS, Family, Mk, args, call, import};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[divide_loop, conditional];

use types::{I8, I64, I128};

/// u0 `(n, d, x)`: for `i` in `0..n & 31`, divides `x` by `d + i`: signed in u0 on odd
/// iterations, unsigned in u1 on even ones; so `d = -i` traps at iteration `i`, in u1
/// when it is even, and `x = MIN` with `d = -1 - i` traps there for overflow when `i` is
/// odd.
pub fn divide_loop(mk: &Mk) -> Family {
    let pair = mk.sig_of(&[I64, I64], &[I64]);
    let entry = mk.func(0, mk.sig_of(&[I64, I64, I64], &[I64]), |b, v| {
        let callee = import(b, FUNCS, 1, pair.clone(), false);
        let head = b.create_block();
        let i = b.append_block_param(head, I64);
        let acc = b.append_block_param(head, I64);
        let body = b.create_block();
        let (odd, even) = (b.create_block(), b.create_block());
        let latch = b.create_block();
        let next = b.append_block_param(latch, I64);
        let exit = b.create_block();

        let n = b.ins().band_imm(v[0], 31);
        let zero = b.ins().iconst(I64, 0);
        b.ins().jump(head, &args(&[zero, zero]));

        b.switch_to_block(head);
        let more = b.ins().icmp(IntCC::SignedLessThan, i, n);
        b.ins().brif(more, body, &[], exit, &[]);

        b.switch_to_block(body);
        let divisor = b.ins().iadd(v[1], i);
        let low = b.ins().band_imm(i, 1);
        b.ins().brif(low, odd, &[], even, &[]);

        b.switch_to_block(odd);
        let q = b.ins().sdiv(v[2], divisor);
        let sum = b.ins().iadd(acc, q);
        b.ins().jump(latch, &args(&[sum]));

        b.switch_to_block(even);
        let q = call(b, callee, &[v[2], divisor])[0];
        let sum = b.ins().bxor(acc, q);
        b.ins().jump(latch, &args(&[sum]));

        b.switch_to_block(latch);
        let i1 = b.ins().iadd_imm(i, 1);
        b.ins().jump(head, &args(&[i1, next]));

        b.switch_to_block(exit);
        b.ins().return_(&[acc]);
    });
    let callee = mk.func(1, pair, |b, v| {
        let q = b.ins().udiv(v[0], v[1]);
        let r = b.ins().urem(v[0], v[1]);
        let out = b.ins().iadd(q, r);
        b.ins().return_(&[out]);
    });
    Family::new("traps/divide_loop", vec![entry, callee])
}

/// u0 `(x, y, c, w)`: traps if `x > y` (unsigned), if `c` (an `i8`) is zero, or if `w`
/// (an `i128`) is not; otherwise returns `x + y`.
pub fn conditional(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64, I8, I128], &[I64]), |b, v| {
        let above = b.ins().icmp(IntCC::UnsignedGreaterThan, v[0], v[1]);
        b.ins().trapnz(above, ir::TrapCode::unwrap_user(1));
        b.ins().trapz(v[2], ir::TrapCode::unwrap_user(2));
        b.ins().trapnz(v[3], ir::TrapCode::INTEGER_OVERFLOW);
        let out = b.ins().iadd(v[0], v[1]);
        b.ins().return_(&[out]);
    });
    Family::new("traps/conditional", vec![entry])
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::data_value::DataValue;

    use super::super::world::{In, Opts, check, host_mk, i64s, random_inputs};

    #[test]
    fn a_divide_traps_deep_in_a_loop() {
        let Some(mk) = host_mk() else { return };
        let family = super::divide_loop(&mk);
        let mut inputs = vec![
            // Zero at i = 4 (u1's udiv) and at i = 5 (u0's sdiv).
            i64s(&[20, -4, 100]),
            i64s(&[20, -5, 100]),
            // MIN / -1 at i = 11, in u0.
            i64s(&[20, -12, i64::MIN]),
            // Past the loop's end, so no trap: zero at i = 12 of 10.
            i64s(&[10, -12, 7]),
            i64s(&[3, 5, 1]),
            i64s(&[0, 0, 5]),
            i64s(&[31, 1, i64::MIN]),
        ];
        inputs.extend(random_inputs(50, &family, 16));
        check(&mk, &family, &inputs, &Opts::forked());
    }

    #[test]
    fn trapnz_and_trapz_on_each_width() {
        let Some(mk) = host_mk() else { return };
        let family = super::conditional(&mk);
        let mut inputs = Vec::new();
        for (x, y) in [(1i64, 2i64), (2, 1), (0, 0), (-1, 0)] {
            for c in [0i8, 1, -128] {
                for w in [0i128, 1, 1 << 64, -1] {
                    inputs.push(vec![
                        In::V(DataValue::I64(x)),
                        In::V(DataValue::I64(y)),
                        In::V(DataValue::I8(c)),
                        In::V(DataValue::I128(w)),
                    ]);
                }
            }
        }
        inputs.extend(random_inputs(51, &family, 8));
        check(&mk, &family, &inputs, &Opts::forked());
    }
}
