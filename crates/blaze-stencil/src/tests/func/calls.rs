//! Calls between the two backends, both ways: direct calls of a `u0:i` name (through the
//! GOT and, `colocated`, PC-relative), `func_addr` with `call_indirect`, a function pointer
//! passed in as an argument and called back, and `return_call` (Cranelift takes it only in
//! the `tail` convention, so those families are `tail` functions called from the host's).

use cranelift_codegen::{
    ir::{self, InstBuilder, types},
    isa::CallConv,
};
use cranelift_frontend::FunctionBuilder;

use super::{FUNCS, Family, Mk, call, call_indirect, import, mix};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[direct, indirect, pointer, callback, tail, tail_stack];

const I64: ir::Type = types::I64;

/// `(x, y) -> 3x - y`.
fn triple(mk: &Mk, index: u32, cc: CallConv) -> ir::Function {
    mk.func(index, mk.sig_in(cc, &[ir::AbiParam::new(I64); 2], &[ir::AbiParam::new(I64)]), |b, p| {
        let three = b.ins().imul_imm(p[0], 3);
        let out = b.ins().isub(three, p[1]);
        b.ins().return_(&[out]);
    })
}

/// `(x, y) -> rotl(x, y) ^ y`.
fn rotate(mk: &Mk, index: u32, cc: CallConv) -> ir::Function {
    mk.func(index, mk.sig_in(cc, &[ir::AbiParam::new(I64); 2], &[ir::AbiParam::new(I64)]), |b, p| {
        let rotated = b.ins().rotl(p[0], p[1]);
        let out = b.ins().bxor(rotated, p[1]);
        b.ins().return_(&[out]);
    })
}

fn pair(mk: &Mk) -> ir::Signature {
    mk.sig_of(&[I64, I64], &[I64])
}

/// u0 calls u1 through the GOT, u2 directly, then u1 again.
pub fn direct(mk: &Mk) -> Family {
    let entry = mk.func(0, pair(mk), |b, p| {
        let far = import(b, FUNCS, 1, pair(mk), false);
        let near = import(b, FUNCS, 2, pair(mk), true);
        let r = call(b, far, &[p[0], p[1]])[0];
        let s = call(b, near, &[r, p[0]])[0];
        let t = call(b, far, &[s, p[1]])[0];
        b.ins().return_(&[t]);
    });
    Family::new("calls/direct", vec![entry, triple(mk, 1, mk.cc()), rotate(mk, 2, mk.cc())])
}

/// u0 takes the addresses of u1 (through the GOT) and u2 (PC-relative) and calls both.
pub fn indirect(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64], &[I64]), |b, p| {
        let far = import(b, FUNCS, 1, pair(mk), false);
        let near = import(b, FUNCS, 2, pair(mk), true);
        let sig = b.import_signature(pair(mk));
        let f = b.ins().func_addr(I64, far);
        let g = b.ins().func_addr(I64, near);
        let r = call_indirect(b, sig, f, &[p[0], p[0]])[0];
        let s = call_indirect(b, sig, g, &[r, p[0]])[0];
        b.ins().return_(&[s]);
    });
    Family::new("calls/indirect", vec![entry, triple(mk, 1, mk.cc()), rotate(mk, 2, mk.cc())])
}

/// u0 calls the two function pointers it is given (any of u1, u2) in turn.
pub fn pointer(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64; 4], &[I64]), |b, p| {
        let sig = b.import_signature(pair(mk));
        let r = call_indirect(b, sig, p[0], &[p[2], p[3]])[0];
        let s = call_indirect(b, sig, p[1], &[r, p[3]])[0];
        b.ins().return_(&[s]);
    });
    Family::new("calls/pointer", vec![entry, triple(mk, 1, mk.cc()), rotate(mk, 2, mk.cc())])
}

/// u0 passes u2's address to u1, which calls it back.
pub fn callback(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64], &[I64]), |b, p| {
        let back = import(b, FUNCS, 2, pair(mk), false);
        let middle = import(b, FUNCS, 1, pair(mk), false);
        let f = b.ins().func_addr(I64, back);
        let r = call(b, middle, &[f, p[0]])[0];
        b.ins().return_(&[r]);
    });
    let middle = mk.func(1, pair(mk), |b, p| {
        let sig = b.import_signature(pair(mk));
        let r = call_indirect(b, sig, p[0], &[p[1], p[1]])[0];
        let out = b.ins().iadd_imm(r, 1);
        b.ins().return_(&[out]);
    });
    Family::new("calls/callback", vec![entry, middle, rotate(mk, 2, mk.cc())])
}

fn tail_family(mk: &Mk, name: &'static str, funcs: Vec<ir::Function>) -> Family {
    let family = Family::new(name, funcs);
    if mk.windows() { family.either() } else { family }
}

/// `tail` functions: u0 `return_call`s u1, which returns or `return_call_indirect`s u2.
pub fn tail(mk: &Mk) -> Family {
    let tail = mk.sig_in(CallConv::Tail, &[ir::AbiParam::new(I64); 2], &[ir::AbiParam::new(I64)]);
    let entry = mk.func(0, tail.clone(), |b, p| {
        let next = import(b, FUNCS, 1, tail.clone(), false);
        let c = b.ins().iadd(p[0], p[1]);
        b.ins().return_call(next, &[c, p[0]]);
    });
    let middle = mk.func(1, tail.clone(), |b, p| {
        let last = import(b, FUNCS, 2, tail.clone(), true);
        let sig = b.import_signature(tail.clone());
        let (done, on) = (b.create_block(), b.create_block());
        let big = b.ins().icmp_imm(ir::condcodes::IntCC::SignedGreaterThan, p[0], 1000);
        b.ins().brif(big, done, &[], on, &[]);
        b.switch_to_block(done);
        let out = b.ins().isub(p[0], p[1]);
        b.ins().return_(&[out]);
        b.switch_to_block(on);
        let f = b.ins().func_addr(I64, last);
        let twice = b.ins().iadd(p[0], p[0]);
        b.ins().return_call_indirect(sig, f, &[twice, p[1]]);
    });
    tail_family(mk, "calls/tail", vec![entry, middle, rotate(mk, 2, CallConv::Tail)])
}

/// `tail` functions whose stack arguments grow and shrink: u0 (2 arguments)
/// `return_call`s u1 (12, past the registers), which `return_call`s u2 (3).
pub fn tail_stack(mk: &Mk) -> Family {
    let sig = |n: usize| mk.sig_in(CallConv::Tail, &vec![ir::AbiParam::new(I64); n], &[ir::AbiParam::new(I64)]);
    let entry = mk.func(0, sig(2), |b, p| {
        let wide = import(b, FUNCS, 1, sig(12), false);
        let mut args = Vec::with_capacity(12);
        for k in 0..12 {
            let v = if k % 2 == 0 { p[0] } else { p[1] };
            args.push(b.ins().iadd_imm(v, k * 0x1111));
        }
        b.ins().return_call(wide, &args);
    });
    let wide = mk.func(1, sig(12), |b, p| {
        let narrow = import(b, FUNCS, 2, sig(3), false);
        let acc = fold(b, p);
        b.ins().return_call(narrow, &[acc, p[11], p[0]]);
    });
    let narrow = mk.func(2, sig(3), |b, p| {
        let acc = fold(b, p);
        b.ins().return_(&[acc]);
    });
    tail_family(mk, "calls/tail_stack", vec![entry, wide, narrow])
}

/// Every one of `values` mixed into one, in order.
fn fold(b: &mut FunctionBuilder<'_>, values: &[ir::Value]) -> ir::Value {
    let mut acc = values[0];
    for (k, value) in values.iter().enumerate().skip(1) {
        acc = mix(b, acc, *value, k as i64);
    }
    acc
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use super::super::world::{In, Opts, check, host_mk, i64s, random_inputs};

    fn edges() -> Vec<Vec<In>> {
        let mut out = Vec::new();
        for (a, b) in [(0, 0), (1, -1), (i64::MAX, i64::MIN), (1001, 3), (-5, 70), (999, 2)] {
            out.push(i64s(&[a, b]));
        }
        out
    }

    #[test]
    fn direct_calls_cross_both_ways() {
        let Some(mk) = host_mk() else { return };
        let family = super::direct(&mk);
        let mut inputs = edges();
        inputs.extend(random_inputs(1, &family, 16));
        check(&mk, &family, &inputs, &Opts::interp());
    }

    #[test]
    fn func_addr_calls_cross_both_ways() {
        let Some(mk) = host_mk() else { return };
        let family = super::indirect(&mk);
        check(&mk, &family, &random_inputs(2, &family, 16), &Opts::plain());
    }

    #[test]
    fn pointers_passed_in_call_either_backend() {
        let Some(mk) = host_mk() else { return };
        let family = super::pointer(&mk);
        let mut inputs = Vec::new();
        for (f, g) in [(1, 2), (2, 1), (1, 1), (2, 2)] {
            for (a, b) in [(3, 4), (-1, 63), (i64::MIN, 65)] {
                let mut input = vec![In::Unit(f), In::Unit(g)];
                input.extend(i64s(&[a, b]));
                inputs.push(input);
            }
        }
        check(&mk, &family, &inputs, &Opts::plain());
    }

    #[test]
    fn a_pointer_passed_down_is_called_back() {
        let Some(mk) = host_mk() else { return };
        let family = super::callback(&mk);
        check(&mk, &family, &random_inputs(3, &family, 16), &Opts::plain());
    }

    #[test]
    fn return_calls_cross_both_ways() {
        let Some(mk) = host_mk() else { return };
        let family = super::tail(&mk);
        let mut inputs = edges();
        inputs.extend(random_inputs(4, &family, 16));
        check(&mk, &family, &inputs, &Opts::plain());
    }

    #[test]
    fn return_calls_grow_and_shrink_stack_arguments() {
        let Some(mk) = host_mk() else { return };
        let family = super::tail_stack(&mk);
        check(&mk, &family, &random_inputs(5, &family, 16), &Opts::plain());
    }
}
