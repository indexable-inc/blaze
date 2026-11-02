//! Control flow in whole functions: a tree of blocks joined by a block with parameters,
//! block arguments that swap, rotate and duplicate parameters of every slot size around a
//! loop, nested loops and a block that branches to itself (whose back edges stencil code
//! counts in its tier cell, checked against the iterations the arguments ask for, and the
//! calls in the other cell), `br_table` with in-range and out-of-range indices, and `brif`
//! on conditions of each width.

use cranelift_codegen::ir::{self, InstBuilder, condcodes::FloatCC, condcodes::IntCC, types};

use super::{FUNCS, Family, Mk, args, call, import, mix};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[blocks, swaps, loops, table, table_loop, brifs];

use types::{F64, I8, I32, I64, I128};

/// Eleven blocks: a tree of `brif`s on the arguments, every leaf jumping to one join with
/// its own two arguments (one leaf passes them swapped).
pub fn blocks(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64, I32], &[I64]), |b, p| {
        let (a, bb, c) = (p[0], p[1], p[2]);
        let [l, r, ll, lr, lrr, rl, rr, rrt, rrf, join] = [(); 10].map(|_| b.create_block());
        let x = b.append_block_param(join, I64);
        let y = b.append_block_param(join, I64);
        let negative = b.ins().icmp_imm(IntCC::SignedLessThan, a, 0);
        b.ins().brif(negative, l, &[], r, &[]);

        b.switch_to_block(l);
        let low = b.ins().band_imm(bb, 1);
        let even = b.ins().icmp_imm(IntCC::Equal, low, 0);
        b.ins().brif(even, ll, &[], lr, &[]);

        b.switch_to_block(ll);
        let product = b.ins().imul(a, bb);
        b.ins().jump(join, &args(&[product, a]));

        b.switch_to_block(lr);
        let difference = b.ins().isub(a, bb);
        let big = b.ins().icmp_imm(IntCC::SignedGreaterThan, c, 100);
        b.ins().brif(big, join, &args(&[difference, bb]), lrr, &[]);

        b.switch_to_block(lrr);
        let rotated = b.ins().rotl(a, c);
        b.ins().jump(join, &args(&[rotated, bb]));

        b.switch_to_block(r);
        let above = b.ins().icmp(IntCC::UnsignedGreaterThan, bb, a);
        b.ins().brif(above, rl, &[], rr, &[]);

        b.switch_to_block(rl);
        let xor = b.ins().bxor(a, bb);
        b.ins().jump(join, &args(&[xor, a]));

        b.switch_to_block(rr);
        b.ins().brif(c, rrt, &[], rrf, &[]);

        b.switch_to_block(rrt);
        let wide = b.ins().sextend(I64, c);
        let sum = b.ins().iadd(a, wide);
        b.ins().jump(join, &args(&[sum, bb]));

        b.switch_to_block(rrf);
        b.ins().jump(join, &args(&[bb, a]));

        b.switch_to_block(join);
        let out = mix(b, x, y, 3);
        b.ins().return_(&[out]);
    });
    Family::new("flow/blocks", vec![entry])
}

/// A loop of `n & 63` iterations whose back edges move its parameters: on odd iterations
/// `(x, y) -> (y, x)`, `(f, g) -> (g, f)` and `(p, q) -> (q, p)`; on even ones the cycle
/// `(x, y, z) -> (y, z, x)`; at iteration 4, `(x, y, z) -> (x, x, y)` and `(f, g) -> (f, f)`.
pub fn swaps(mk: &Mk) -> Family {
    let sig = mk.sig_of(&[I64, I64, I64, I64, F64, F64, I128, I128], &[I64, F64, I128]);
    let entry = mk.func(0, sig, |b, v| {
        let head = b.create_block();
        let tys = [I64, I64, I64, I64, F64, F64, I128, I128];
        let h: Vec<ir::Value> = tys.iter().map(|ty| b.append_block_param(head, *ty)).collect();
        let [body, dup, odd_test, odd, even, exit] = [(); 6].map(|_| b.create_block());
        let k = b.ins().band_imm(v[3], 63);
        b.ins().jump(head, &args(&[k, v[0], v[1], v[2], v[4], v[5], v[6], v[7]]));

        b.switch_to_block(head);
        let (i, x, y, z, f, g, p, q) = (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        b.ins().brif(i, body, &[], exit, &[]);

        b.switch_to_block(body);
        let j = b.ins().iadd_imm(i, -1);
        let four = b.ins().icmp_imm(IntCC::Equal, i, 4);
        b.ins().brif(four, dup, &[], odd_test, &[]);

        b.switch_to_block(dup);
        b.ins().jump(head, &args(&[j, x, x, y, f, f, p, q]));

        b.switch_to_block(odd_test);
        let low = b.ins().band_imm(i, 1);
        b.ins().brif(low, odd, &[], even, &[]);

        b.switch_to_block(odd);
        b.ins().jump(head, &args(&[j, y, x, z, g, f, q, p]));

        b.switch_to_block(even);
        b.ins().jump(head, &args(&[j, y, z, x, f, g, p, q]));

        b.switch_to_block(exit);
        let r = mix(b, x, y, 1);
        let r = mix(b, r, z, 2);
        let d = b.ins().fsub(f, g);
        let one = b.ins().iconst(I64, 1);
        let rotated = b.ins().rotl(q, one);
        let pq = b.ins().bxor(p, rotated);
        b.ins().return_(&[r, d, pq]);
    });
    Family::new("flow/swaps", vec![entry]).refused(mk.windows())
}

/// The back edges [`swaps`] takes for `n`.
pub fn swaps_back_edges(n: i64) -> u64 {
    (n & 63) as u64
}

/// Nested loops, `n & 15` by `m & 15`, the inner one calling u1, then a block that
/// branches to itself `k & 31` times.
pub fn loops(mk: &Mk) -> Family {
    let step = mk.sig_of(&[I64, I64], &[I64]);
    let entry = mk.func(0, mk.sig_of(&[I64, I64, I64], &[I64]), |b, v| {
        let callee = import(b, FUNCS, 1, step.clone(), false);
        let outer = b.create_block();
        let i = b.append_block_param(outer, I64);
        let acc = b.append_block_param(outer, I64);
        let outer_body = b.create_block();
        let inner = b.create_block();
        let j = b.append_block_param(inner, I64);
        let acc2 = b.append_block_param(inner, I64);
        let inner_body = b.create_block();
        let latch = b.create_block();
        let spin = b.create_block();
        let t = b.append_block_param(spin, I64);
        let acc4 = b.append_block_param(spin, I64);
        let exit = b.create_block();
        let r = b.append_block_param(exit, I64);

        let n = b.ins().band_imm(v[0], 15);
        let m = b.ins().band_imm(v[1], 15);
        let k = b.ins().band_imm(v[2], 31);
        let zero = b.ins().iconst(I64, 0);
        let seed = b.ins().bxor(v[0], v[1]);
        b.ins().jump(outer, &args(&[zero, seed]));

        b.switch_to_block(outer);
        let more = b.ins().icmp(IntCC::SignedLessThan, i, n);
        b.ins().brif(more, outer_body, &[], spin, &args(&[k, acc]));

        b.switch_to_block(outer_body);
        let zero = b.ins().iconst(I64, 0);
        b.ins().jump(inner, &args(&[zero, acc]));

        b.switch_to_block(inner);
        let more = b.ins().icmp(IntCC::SignedLessThan, j, m);
        b.ins().brif(more, inner_body, &[], latch, &[]);

        b.switch_to_block(inner_body);
        let row = b.ins().ishl_imm(i, 4);
        let at = b.ins().iadd(row, j);
        let acc3 = call(b, callee, &[acc2, at])[0];
        let j1 = b.ins().iadd_imm(j, 1);
        b.ins().jump(inner, &args(&[j1, acc3]));

        b.switch_to_block(latch);
        let i1 = b.ins().iadd_imm(i, 1);
        b.ins().jump(outer, &args(&[i1, acc2]));

        b.switch_to_block(spin);
        let t1 = b.ins().iadd_imm(t, -1);
        let tripled = b.ins().imul_imm(acc4, 3);
        let acc5 = b.ins().iadd_imm(tripled, 1);
        b.ins().brif(t, spin, &args(&[t1, acc5]), exit, &args(&[acc4]));

        b.switch_to_block(exit);
        b.ins().return_(&[r]);
    });
    let callee = mk.func(1, step.clone(), |b, v| {
        let five = b.ins().iconst(I64, 5);
        let rotated = b.ins().rotl(v[0], five);
        let mixed = b.ins().bxor(rotated, v[1]);
        let out = b.ins().iadd_imm(mixed, 1);
        b.ins().return_(&[out]);
    });
    Family::new("flow/loops", vec![entry, callee])
}

/// What [`loops`]' two functions count for `(n, m, k)`.
pub fn loops_counts(n: i64, m: i64, k: i64) -> Vec<[u64; 2]> {
    let (n, m, k) = ((n & 15) as u64, (m & 15) as u64, (k & 31) as u64);
    vec![[1, n * m + n + k], [n * m, 0]]
}

/// `br_table` of six entries (one block twice) and a default, each arm its own.
pub fn table(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I32, I64], &[I64]), |b, v| {
        let y = v[1];
        let arms = [(); 5].map(|_| b.create_block());
        let fallback = b.create_block();
        let join = b.create_block();
        let joined = b.append_block_param(join, I64);
        let pool = &mut b.func.dfg;
        let default = pool.block_call(fallback, &[]);
        let entries: Vec<ir::BlockCall> =
            [0, 1, 2, 1, 3, 4].iter().map(|k| pool.block_call(arms[*k], &[])).collect();
        let jt = b.create_jump_table(ir::JumpTableData::new(default, &entries));
        b.ins().br_table(v[0], jt);

        for (k, arm) in arms.iter().enumerate() {
            b.switch_to_block(*arm);
            let out = match k {
                0 => b.ins().iadd_imm(y, 100),
                1 => b.ins().imul_imm(y, 3),
                2 => b.ins().bxor_imm(y, 0x5555),
                3 => {
                    let wide = b.ins().sextend(I64, v[0]);
                    b.ins().isub(y, wide)
                }
                _ => b.ins().ushr_imm(y, 3),
            };
            b.ins().jump(join, &args(&[out]));
        }
        b.switch_to_block(fallback);
        let wide = b.ins().uextend(I64, v[0]);
        let out = mix(b, y, wide, 9);
        b.ins().return_(&[out]);

        b.switch_to_block(join);
        let out = b.ins().iadd_imm(joined, 1);
        b.ins().return_(&[out]);
    });
    Family::new("flow/table", vec![entry])
}

/// A loop of `n & 15` iterations whose body is a `br_table` on `i & 7` (five entries, so
/// three of the eight indices take the default), every arm ending at the latch.
pub fn table_loop(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64], &[I64]), |b, v| {
        let head = b.create_block();
        let i = b.append_block_param(head, I64);
        let acc = b.append_block_param(head, I64);
        let body = b.create_block();
        let arms = [(); 5].map(|_| b.create_block());
        let fallback = b.create_block();
        let latch = b.create_block();
        let next = b.append_block_param(latch, I64);
        let exit = b.create_block();

        let n = b.ins().band_imm(v[0], 15);
        let zero = b.ins().iconst(I64, 0);
        b.ins().jump(head, &args(&[zero, v[1]]));

        b.switch_to_block(head);
        let more = b.ins().icmp(IntCC::SignedLessThan, i, n);
        b.ins().brif(more, body, &[], exit, &[]);

        b.switch_to_block(body);
        let low = b.ins().band_imm(i, 7);
        let index = b.ins().ireduce(I32, low);
        let pool = &mut b.func.dfg;
        let default = pool.block_call(fallback, &[]);
        let entries: Vec<ir::BlockCall> = arms.iter().map(|arm| pool.block_call(*arm, &[])).collect();
        let jt = b.create_jump_table(ir::JumpTableData::new(default, &entries));
        b.ins().br_table(index, jt);

        for (k, arm) in arms.iter().enumerate() {
            b.switch_to_block(*arm);
            let out = mix(b, acc, i, k as i64 * 17 + 1);
            b.ins().jump(latch, &args(&[out]));
        }
        b.switch_to_block(fallback);
        let out = b.ins().imul_imm(acc, 31);
        b.ins().jump(latch, &args(&[out]));

        b.switch_to_block(latch);
        let i1 = b.ins().iadd_imm(i, 1);
        b.ins().jump(head, &args(&[i1, next]));

        b.switch_to_block(exit);
        b.ins().return_(&[acc]);
    });
    Family::new("flow/table_loop", vec![entry])
}

/// A chain of `brif`s on an `i64` compare, an `i8`, an `i128`, an `f64` compare, and two
/// more compares, each arm returning its own value.
pub fn brifs(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I8, I128, F64], &[I64]), |b, v| {
        let (x, y, z, f) = (v[0], v[1], v[2], v[3]);
        let tests = [(); 5].map(|_| b.create_block());
        let arms = [(); 7].map(|_| b.create_block());

        let one = b.ins().icmp_imm(IntCC::Equal, x, 1);
        b.ins().brif(one, arms[0], &[], tests[0], &[]);

        b.switch_to_block(tests[0]);
        b.ins().brif(y, arms[1], &[], tests[1], &[]);

        b.switch_to_block(tests[1]);
        b.ins().brif(z, arms[2], &[], tests[2], &[]);

        b.switch_to_block(tests[2]);
        let zero = b.ins().f64const(0.0);
        let negative = b.ins().fcmp(FloatCC::LessThan, f, zero);
        b.ins().brif(negative, arms[3], &[], tests[3], &[]);

        b.switch_to_block(tests[3]);
        let small = b.ins().icmp_imm(IntCC::UnsignedLessThan, x, 1000);
        b.ins().brif(small, arms[4], &[], tests[4], &[]);

        b.switch_to_block(tests[4]);
        let unordered = b.ins().fcmp(FloatCC::Unordered, f, f);
        b.ins().brif(unordered, arms[5], &[], arms[6], &[]);

        for (k, arm) in arms.iter().enumerate() {
            b.switch_to_block(*arm);
            let k = b.ins().iconst(I64, k as i64 + 1);
            let out = mix(b, x, k, 0x1234);
            b.ins().return_(&[out]);
        }
    });
    Family::new("flow/brifs", vec![entry])
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::{
        data_value::DataValue,
        ir::immediates::Ieee64,
    };

    use super::super::world::{In, Opts, check, check_counted, host_mk, i64s, random_inputs};

    fn int(input: &[In], k: usize) -> i64 {
        match &input[k] {
            In::V(DataValue::I64(it)) => *it,
            other => panic!("argument {k} is {other:?}, not an i64"),
        }
    }

    #[test]
    fn a_tree_of_blocks() {
        let Some(mk) = host_mk() else { return };
        let family = super::blocks(&mk);
        let mut inputs = random_inputs(30, &family, 64);
        for (a, b, c) in [(-1, 2, 0), (-1, 3, 101), (-1, 3, 5), (1, 2, 0), (1, 0, 1), (0, 0, 0)] {
            inputs.push(vec![In::V(DataValue::I64(a)), In::V(DataValue::I64(b)), In::V(DataValue::I32(c))]);
        }
        check(&mk, &family, &inputs, &Opts::interp());
    }

    #[test]
    fn block_arguments_swap_rotate_and_duplicate() {
        let Some(mk) = host_mk() else { return };
        let family = super::swaps(&mk);
        let mut inputs = random_inputs(31, &family, 24);
        // 0 to 7 iterations: none, fewer than reach the duplicating edge (at i == 4), and
        // that edge first, in the middle and near the end.
        for (n, input) in inputs[..8].iter_mut().enumerate() {
            input[3] = In::V(DataValue::I64(n as i64));
        }
        check_counted(&mk, &family, &inputs, &Opts::interp(), |input| {
            vec![[1, super::swaps_back_edges(int(input, 3))]]
        });
    }

    #[test]
    fn loops_count_their_back_edges_and_calls() {
        let Some(mk) = host_mk() else { return };
        let family = super::loops(&mk);
        let mut inputs = vec![i64s(&[0, 0, 0]), i64s(&[1, 1, 1]), i64s(&[15, 15, 31]), i64s(&[3, 0, 2])];
        inputs.extend(random_inputs(32, &family, 16));
        check_counted(&mk, &family, &inputs, &Opts::interp(), |input| {
            super::loops_counts(int(input, 0), int(input, 1), int(input, 2))
        });
    }

    #[test]
    fn br_table_in_range_and_out_of_range() {
        let Some(mk) = host_mk() else { return };
        let family = super::table(&mk);
        let mut inputs = Vec::new();
        for x in [0, 1, 2, 3, 4, 5, 6, 7, 100, -1, i32::MIN, i32::MAX] {
            for y in [0, -7, 0x1234_5678_9abc] {
                inputs.push(vec![In::V(DataValue::I32(x)), In::V(DataValue::I64(y))]);
            }
        }
        check(&mk, &family, &inputs, &Opts::interp());
    }

    #[test]
    fn br_table_in_a_counted_loop() {
        let Some(mk) = host_mk() else { return };
        let family = super::table_loop(&mk);
        let mut inputs = vec![i64s(&[0, 1]), i64s(&[15, -1]), i64s(&[8, 3])];
        inputs.extend(random_inputs(33, &family, 16));
        check_counted(&mk, &family, &inputs, &Opts::interp(), |input| {
            vec![[1, (int(input, 0) & 15) as u64]]
        });
    }

    #[test]
    fn brif_on_each_width() {
        let Some(mk) = host_mk() else { return };
        let family = super::brifs(&mk);
        let mut inputs = random_inputs(34, &family, 48);
        for (x, y, z, f) in [
            (1i64, 0i8, 0i128, 1.0f64),
            (2, 1, 0, 1.0),
            (2, 0, 1 << 100, 1.0),
            (2, 0, 0, -1.0),
            (5, 0, 0, 1.0),
            (5000, 0, 0, f64::NAN),
            (5000, 0, 0, 2.0),
        ] {
            inputs.push(vec![
                In::V(DataValue::I64(x)),
                In::V(DataValue::I8(y)),
                In::V(DataValue::I128(z)),
                In::V(DataValue::F64(Ieee64::with_bits(f.to_bits()))),
            ]);
        }
        check(&mk, &family, &inputs, &Opts::interp());
    }
}
