//! Tail calls in the target's own convention, as the IR builder writes Rust's `become`:
//! `return_call` and `return_call_indirect` between functions of `system_v`, `apple_aarch64`
//! or `windows_fastcall`, whose caller's caller reserved the stack arguments and pops them.
//! Cranelift refuses them (its verifier tail-calls only in `tail`), so these functions are
//! not families of [`super::families`], held to Cranelift's acceptance and code: they are
//! stencil code alone, compiled for every target, refused where the backend must refuse, and
//! on the host run against a model of what they compute.

use cranelift_codegen::{
    ir::{
        self, AbiParam, ArgumentPurpose, InstBuilder, Opcode,
        condcodes::IntCC,
        types::{I32, I64},
    },
    isa::CallConv,
};

use super::{FUNCS, Mk, import, mem, mix};
use crate::tests::host;

/// The hops' parameters: the hops left, the accumulator, eight carried values `a` to `h`,
/// and the first hop's frame pointer (0 until it is known).
const PARAMS: usize = 11;

/// Where each hop sends the carried values (`next[k] = carried[PERMS[hop][k]]`): hop 0 swaps
/// `a` and `b`, `e` and `f`, `g` and `h`; hop 1 rotates `e` to `h`. System V passes `e` and
/// the rest on the stack, AAPCS64 and Apple's `g` and the rest, so stack arguments trade
/// places with each other and with registers.
const PERMS: [[usize; 8]; 2] = [[1, 0, 2, 3, 5, 4, 7, 6], [0, 1, 2, 3, 5, 6, 7, 4]];

fn hop_sig(mk: &Mk) -> ir::Signature {
    mk.sig(&[AbiParam::new(I64); PARAMS], &[AbiParam::new(I64); 2])
}

/// Hop `index` (0 or 1). With no hops left it returns the accumulator mixed with every
/// carried value, and its frame pointer less the first hop's: 0 when no hop's frame stayed
/// on the stack. Otherwise it mixes `e` and `h` into the accumulator and tail-calls the other
/// hop with the carried values moved as [`PERMS`] says: hop 0 through the GOT, hop 1 through
/// `func_addr`.
fn hop(mk: &Mk, index: u32) -> ir::Function {
    let sig = hop_sig(mk);
    mk.func(index, sig.clone(), |b, p| {
        let fp = b.ins().get_frame_pointer(I64);
        let first = b.ins().icmp_imm_s(IntCC::Equal, p[10], 0);
        let fp0 = b.ins().select(first, fp, p[10]);
        let (done, on) = (b.create_block(), b.create_block());
        let last = b.ins().icmp_imm_s(IntCC::Equal, p[0], 0);
        b.ins().brif(last, done, &[], on, &[]);
        b.switch_to_block(done);
        let mut acc = p[1];
        for (k, value) in p[2..10].iter().enumerate() {
            acc = mix(b, acc, *value, k as i64);
        }
        let moved = b.ins().isub(fp, fp0);
        b.ins().return_(&[acc, moved]);
        b.switch_to_block(on);
        let n = b.ins().iadd_imm_s(p[0], -1);
        let acc = mix(b, p[1], p[6], 1 + i64::from(index));
        let acc = mix(b, acc, p[9], 3);
        let mut args = vec![n, acc];
        args.extend(PERMS[index as usize].iter().map(|&k| p[2 + k]));
        args.push(fp0);
        if index == 0 {
            let next = import(b, FUNCS, 1, sig.clone(), false);
            b.ins().return_call(next, &args);
        } else {
            let next = import(b, FUNCS, 0, sig.clone(), true);
            let sig_ref = b.import_signature(sig.clone());
            let f = b.ins().func_addr(I64, next);
            b.ins().return_call_indirect(sig_ref, f, &args);
        }
    })
}

/// u0 `(sret, x, y)` tail-calls u1 with its own StructReturn pointer and `(3x + 1, x ^ y)`;
/// u1 writes its two values through the pointer.
fn struct_return(mk: &Mk) -> Vec<ir::Function> {
    let sret = AbiParam::special(I64, ArgumentPurpose::StructReturn);
    let sig = mk.sig(&[sret, AbiParam::new(I64), AbiParam::new(I64)], &[]);
    let entry = mk.func(0, sig.clone(), |b, p| {
        let next = import(b, FUNCS, 1, sig.clone(), false);
        let tripled = b.ins().imul_imm_s(p[1], 3);
        let x = b.ins().iadd_imm_s(tripled, 1);
        let y = b.ins().bxor(p[1], p[2]);
        b.ins().return_call(next, &[p[0], x, y]);
    });
    let callee = mk.func(1, sig.clone(), |b, p| {
        b.ins().store(mem(), p[1], p[0], 0);
        b.ins().store(mem(), p[2], p[0], 8);
        b.ins().return_(&[]);
    });
    vec![entry, callee]
}

/// x86-64 (AArch64 has no StructArgument): u0 `(n, s: StructArgument(24), acc)` tail-calls
/// itself `n` times, colocated, forwarding its own StructArgument and mixing the struct's
/// first two words into the accumulator, then returns it mixed with the third.
fn struct_argument(mk: &Mk) -> ir::Function {
    let s = AbiParam::special(I64, ArgumentPurpose::StructArgument(24));
    let sig = mk.sig(&[AbiParam::new(I64), s, AbiParam::new(I64)], &[AbiParam::new(I64)]);
    mk.func(0, sig.clone(), |b, p| {
        let (done, on) = (b.create_block(), b.create_block());
        let last = b.ins().icmp_imm_s(IntCC::Equal, p[0], 0);
        b.ins().brif(last, done, &[], on, &[]);
        b.switch_to_block(done);
        let word = b.ins().load(I64, mem(), p[1], 16);
        let out = mix(b, p[2], word, 0);
        b.ins().return_(&[out]);
        b.switch_to_block(on);
        let this = import(b, FUNCS, 0, sig.clone(), true);
        let n = b.ins().iadd_imm_s(p[0], -1);
        let w0 = b.ins().load(I64, mem(), p[1], 0);
        let w1 = b.ins().load(I64, mem(), p[1], 8);
        let acc = mix(b, p[2], w0, 1);
        let acc = mix(b, acc, w1, 2);
        b.ins().return_call(this, &[n, p[1], acc]);
    })
}

fn return_call_in(func: &ir::Function) -> ir::Inst {
    func.layout
        .blocks()
        .flat_map(|block| func.layout.block_insts(block))
        .find(|&inst| {
            matches!(func.dfg.insts[inst].opcode(), Opcode::ReturnCall | Opcode::ReturnCallIndirect)
        })
        .expect("a return call")
}

/// u0 of `caller` tail-calls u1 of `callee` with `pick(params)`.
fn tail_from(
    mk: &Mk,
    caller: ir::Signature,
    callee: ir::Signature,
    pick: impl FnOnce(&[ir::Value]) -> Vec<ir::Value>,
) -> ir::Function {
    mk.func(0, caller, |b, p| {
        let next = import(b, FUNCS, 1, callee, true);
        let args = pick(p);
        b.ins().return_call(next, &args);
    })
}

#[test]
fn native_tail_calls_compile_for_every_target() {
    for target in host::TARGETS {
        let mk = Mk::new(target);
        let mut funcs = vec![hop(&mk, 0), hop(&mk, 1)];
        funcs.extend(struct_return(&mk));
        if mk.x64() {
            funcs.push(struct_argument(&mk));
        }
        for func in &funcs {
            if let Err(e) = crate::compile(func, &target) {
                panic!("{}: {e}\n{}", host::triple(&target), func.display());
            }
        }
    }
}

#[test]
fn native_tail_calls_the_backend_cannot_make_are_errors_naming_the_call() {
    let p = || AbiParam::new(I64);
    for target in host::TARGETS {
        let mk = Mk::new(target);
        let ret = [p()];
        let narrow = mk.sig(&[p(), p()], &ret);
        let mut refusals = vec![
            (
                "bytes of stack arguments",
                tail_from(&mk, narrow.clone(), mk.sig(&[p(); 12], &ret), |v| {
                    (0..12).map(|k| v[k % 2]).collect()
                }),
            ),
            (
                "convention",
                tail_from(&mk, narrow.clone(), mk.sig_in(CallConv::Tail, &[p(), p()], &ret), |v| {
                    v.to_vec()
                }),
            ),
            (
                "return values must be the caller's",
                tail_from(&mk, narrow.clone(), mk.sig(&[p(), p()], &[AbiParam::new(I32)]), |v| {
                    v.to_vec()
                }),
            ),
        ];
        let sret = AbiParam::special(I64, ArgumentPurpose::StructReturn);
        let with_sret = mk.sig(&[sret, p()], &[]);
        refusals.push((
            "StructReturn pointer",
            tail_from(&mk, with_sret.clone(), with_sret, |v| vec![v[1], v[1]]),
        ));
        if mk.x64() {
            let s = AbiParam::special(I64, ArgumentPurpose::StructArgument(16));
            refusals.push((
                "not the caller's own StructArgument",
                tail_from(&mk, mk.sig(&[p(), s], &ret), mk.sig(&[s, p()], &ret), |v| {
                    vec![v[0], v[0]]
                }),
            ));
        }
        for (says, func) in &refusals {
            let triple = host::triple(&target);
            match crate::compile(func, &target) {
                Ok(_) => panic!(
                    "{triple}: compiled, where it is refused for {says:?}\n{}",
                    func.display()
                ),
                Err(e) => {
                    assert_eq!(e.inst, Some(return_call_in(func)), "{triple}: {e}");
                    assert!(e.what.contains(says), "{triple}: {e}, not {says:?}");
                }
            }
        }
    }
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::{data_value::DataValue, ir};

    use super::{Mk, PERMS, hop, struct_return};
    use crate::{
        code::Code,
        tests::{
            exec::{self, Exec, Link, Unit},
            func::world::host_mk,
            rng::Rng,
        },
    };

    /// [`super::super::mix`] of `i64`s.
    fn mixed(acc: i64, x: i64, k: i64) -> i64 {
        (acc.rotate_left(7) ^ x).wrapping_add(k)
    }

    /// What `n` hops from hop 0 return first.
    fn hops(n: u64, acc: i64, carried: [i64; 8]) -> i64 {
        let (mut acc, mut carried) = (acc, carried);
        for step in 0..n {
            let hop = (step % 2) as usize;
            acc = mixed(mixed(acc, carried[4], 1 + hop as i64), carried[7], 3);
            carried = PERMS[hop].map(|k| carried[k]);
        }
        carried.iter().enumerate().fold(acc, |acc, (k, x)| mixed(acc, *x, k as i64))
    }

    /// What [`super::struct_argument`] returns for `n` hops over the struct's `words`.
    #[cfg(target_arch = "x86_64")]
    fn struct_hops(n: u64, words: [i64; 3], acc: i64) -> i64 {
        let mut acc = acc;
        for _ in 0..n {
            acc = mixed(mixed(acc, words[0], 1), words[1], 2);
        }
        mixed(acc, words[2], 0)
    }

    /// `funcs` compiled by the stencil backend alone and linked, `data` beside them.
    fn link(mk: &Mk, funcs: &[ir::Function], data: &[&[u8]]) -> Exec {
        let codes: Vec<Code> = funcs
            .iter()
            .enumerate()
            .map(|(i, func)| {
                crate::compile(func, &mk.target)
                    .unwrap_or_else(|e| panic!("u0:{i}: {e}\n{}", func.display()))
            })
            .collect();
        let units: Vec<Unit<'_>> = codes
            .iter()
            .zip(funcs)
            .map(|(code, func)| Unit::Stencil(code, &func.signature))
            .collect();
        let isa = exec::host_isa();
        Exec::link(&Link { isa: &*isa, units: &units, data, host: &[] })
    }

    /// The hops' two results for `n` hops from hop 0.
    fn run_hops(exec: &Exec, n: u64, acc: i64, carried: [i64; 8]) -> (i64, i64) {
        let mut args = vec![DataValue::I64(n as i64), DataValue::I64(acc)];
        args.extend(carried.iter().map(|x| DataValue::I64(*x)));
        args.push(DataValue::I64(0));
        let out = exec.values(0, &args);
        match out.as_slice() {
            [DataValue::I64(acc), DataValue::I64(moved)] => (*acc, *moved),
            other => panic!("the hops return {other:?}"),
        }
    }

    /// A million hops whose frames stayed would need far more than a test thread's stack:
    /// they finish, and the last hop's frame pointer is the first's.
    #[test]
    fn native_tail_calls_run_in_constant_stack() {
        let Some(mk) = host_mk() else { return };
        let exec = link(&mk, &[hop(&mk, 0), hop(&mk, 1)], &[]);
        let carried = [1i64, -2, 3, -4, 5, -6, 7, -8]
            .map(|x| x.wrapping_mul(0x9e37_79b9_7f4a_7c15u64 as i64));
        for n in [1 << 20, (1 << 20) + 1] {
            assert_eq!(run_hops(&exec, n, 17, carried), (hops(n, 17, carried), 0), "{n} hops");
        }
    }

    /// Each hop swaps or rotates stack arguments among themselves and with registers: the
    /// incoming ones are read before any outgoing one is written over them.
    #[test]
    fn stack_arguments_swapped_by_tail_calls_arrive_swapped() {
        let Some(mk) = host_mk() else { return };
        let exec = link(&mk, &[hop(&mk, 0), hop(&mk, 1)], &[]);
        let mut rng = Rng::new(40);
        for n in 0..8 {
            for _ in 0..4 {
                let acc = rng.next() as i64;
                let carried = [(); 8].map(|()| rng.next() as i64);
                assert_eq!(
                    run_hops(&exec, n, acc, carried),
                    (hops(n, acc, carried), 0),
                    "{n} hops from {acc}, {carried:?}"
                );
            }
        }
    }

    #[test]
    fn a_struct_return_function_tail_calls_one_with_its_own_pointer() {
        let Some(mk) = host_mk() else { return };
        let area = [0u8; 16];
        let exec = link(&mk, &struct_return(&mk), &[&area[..]]);
        for (x, y) in [(0i64, 0i64), (5, -1), (i64::MAX, i64::MIN)] {
            let at = DataValue::I64(exec.data_addr(0) as i64);
            let out = exec.values(0, &[at, DataValue::I64(x), DataValue::I64(y)]);
            assert!(out.is_empty(), "{out:?}");
            let mut want = x.wrapping_mul(3).wrapping_add(1).to_le_bytes().to_vec();
            want.extend_from_slice(&(x ^ y).to_le_bytes());
            assert_eq!(exec.data(0), want, "on ({x}, {y})");
        }
    }

    /// x86-64 only: AArch64 has no StructArgument.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_forwarded_struct_argument_stays_in_place() {
        let Some(mk) = host_mk() else { return };
        let words = [0x0123_4567_89ab_cdefi64, -3, i64::MIN + 5];
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let exec = link(&mk, &[super::struct_argument(&mk)], &[&bytes[..]]);
        for n in [0u64, 1, 2, 1 << 16] {
            let at = DataValue::I64(exec.data_addr(0) as i64);
            let out = exec.values(0, &[DataValue::I64(n as i64), at, DataValue::I64(9)]);
            assert_eq!(out, [DataValue::I64(struct_hops(n, words, 9))], "{n} hops");
        }
    }
}
