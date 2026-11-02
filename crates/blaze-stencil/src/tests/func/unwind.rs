//! `try_call` with unwinding through both backends: the host's `throw_fn` (`u2:0`, raising
//! an exception whose handler gets the value in payload 0 and the tag in payload 1) is
//! called by a thrower (u2), whose caller (u1) calls it plainly, with no handler, so the
//! unwind passes through u1's frame, realigned by a 64-byte slot, to the `try_call` in u0.
//! The mixes put each of the three in either backend. A tag of 0 throws nothing, so the
//! `try_call` returns normally with its `TryCallRet` values.
//!
//! The families: handlers for two tags (the second one matching), a tag and a default,
//! the same through `try_call_indirect`, and a nested one whose inner `try_call` has a tag
//! handler and a default that cleans up and throws again (the IR builder's cleanup pad,
//! with the host's throw for `_Unwind_Resume`) to the outer one.

use cranelift_codegen::ir::{self, InstBuilder, types};
use cranelift_frontend::FunctionBuilder;

use super::{FUNCS, Family, HOST, Mk, call, import, mem, mix, slot};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[catch_tag, catch_default, catch_indirect, nested];

use types::I64;

/// Where a handler lands: a tag, or every exception.
#[derive(Clone, Copy)]
enum Catch {
    Tag(u32),
    Default,
}

fn throw_sig(mk: &Mk) -> ir::Signature {
    mk.sig_of(&[I64, I64], &[])
}

fn pair_sig(mk: &Mk, params: usize) -> ir::Signature {
    mk.sig_of(&vec![I64; params], &[I64, I64])
}

/// `(tag, value)`: throws `(tag, value)` unless `tag` is 0, when it returns
/// `(3 * value, value ^ 5)`.
fn thrower(mk: &Mk, index: u32) -> ir::Function {
    mk.func(index, pair_sig(mk, 2), |b, v| {
        let throw = import(b, HOST, 0, throw_sig(mk), false);
        let (raise, home) = (b.create_block(), b.create_block());
        b.ins().brif(v[0], raise, &[], home, &[]);
        b.switch_to_block(raise);
        call(b, throw, &[v[0], v[1]]);
        let zero = b.ins().iconst(I64, 0);
        b.ins().return_(&[zero, zero]);
        b.switch_to_block(home);
        let tripled = b.ins().imul_imm(v[1], 3);
        let flipped = b.ins().bxor_imm(v[1], 5);
        b.ins().return_(&[tripled, flipped]);
    })
}

/// `(tag, value)`: calls u2 with `(tag, value + 1)`, no handler, keeping `value` in a
/// 64-aligned slot across the call, and returns `(r + slot, s * 2)`.
fn middle(mk: &Mk, index: u32) -> ir::Function {
    mk.func(index, pair_sig(mk, 2), |b, v| {
        let next = import(b, FUNCS, 2, pair_sig(mk, 2), false);
        let kept = slot(b, 64, 6);
        b.ins().stack_store(I64, v[1], kept, 8);
        let bumped = b.ins().iadd_imm(v[1], 1);
        let rs = call(b, next, &[v[0], bumped]);
        let back = b.ins().stack_load(I64, I64, kept, 8);
        let r = b.ins().iadd(rs[0], back);
        let s = b.ins().imul_imm(rs[1], 2);
        b.ins().return_(&[r, s]);
    })
}

/// A `try_call` (or, given `pointer`, a `try_call_indirect` of it) of `callee` with
/// `args`, whose normal return goes to a new block of its two results and whose handlers
/// go each to a new block of the two payloads. Gives the normal block and the handlers'.
fn try_call(
    b: &mut FunctionBuilder<'_>,
    callee: ir::FuncRef,
    pointer: Option<ir::Value>,
    args: &[ir::Value],
    catches: &[Catch],
) -> (ir::Block, Vec<ir::Block>) {
    let returned = b.create_block();
    b.append_block_param(returned, I64);
    b.append_block_param(returned, I64);
    let normal = b.func.dfg.block_call(returned, &[ir::BlockArg::TryCallRet(0), ir::BlockArg::TryCallRet(1)]);
    let mut landings = Vec::with_capacity(catches.len());
    let mut items = Vec::with_capacity(catches.len());
    for catch in catches {
        let landing = b.create_block();
        b.append_block_param(landing, I64);
        b.append_block_param(landing, I64);
        let to = b.func.dfg.block_call(landing, &[ir::BlockArg::TryCallExn(0), ir::BlockArg::TryCallExn(1)]);
        items.push(match catch {
            Catch::Tag(tag) => ir::ExceptionTableItem::Tag(ir::ExceptionTag::from_u32(*tag), to),
            Catch::Default => ir::ExceptionTableItem::Default(to),
        });
        landings.push(landing);
    }
    let sig = b.func.dfg.ext_funcs[callee].signature;
    let table = b.func.dfg.exception_tables.push(ir::ExceptionTableData::new(sig, normal, items));
    match pointer {
        Some(pointer) => b.ins().try_call_indirect(pointer, args, table),
        None => b.ins().try_call(callee, args, table),
    };
    (returned, landings)
}

/// u0 `(tag, value) -> i64` catching u1's call with `catches`: the normal return gives a
/// mix of its two results, the handler `k` a mix of its payloads with `1000 * (k + 1)`.
fn catcher(mk: &Mk, name: &'static str, catches: &[Catch], indirect: bool) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64], &[I64]), |b, v| {
        let callee = import(b, FUNCS, 1, pair_sig(mk, 2), false);
        let pointer = indirect.then(|| b.ins().func_addr(I64, callee));
        let (returned, landings) = try_call(b, callee, pointer, &[v[0], v[1]], catches);
        b.switch_to_block(returned);
        let rs = b.block_params(returned).to_vec();
        let out = mix(b, rs[0], rs[1], 1);
        b.ins().return_(&[out]);
        for (k, landing) in landings.into_iter().enumerate() {
            b.switch_to_block(landing);
            b.set_cold_block(landing);
            let payload = b.block_params(landing).to_vec();
            let out = mix(b, payload[0], payload[1], 1000 * (k as i64 + 1));
            b.ins().return_(&[out]);
        }
    });
    let family = Family::new(name, vec![entry, middle(mk, 1), thrower(mk, 2)]);
    if mk.windows() { family.either() } else { family }
}

/// Handlers for tags 5 and 7: a 7 matches the second.
pub fn catch_tag(mk: &Mk) -> Family {
    catcher(mk, "unwind/catch_tag", &[Catch::Tag(5), Catch::Tag(7)], false)
}

/// A handler for tag 5 and a default for the rest.
pub fn catch_default(mk: &Mk) -> Family {
    catcher(mk, "unwind/catch_default", &[Catch::Tag(5), Catch::Default], false)
}

/// [`catch_tag`] through `func_addr` and `try_call_indirect`.
pub fn catch_indirect(mk: &Mk) -> Family {
    catcher(mk, "unwind/catch_indirect", &[Catch::Tag(5), Catch::Tag(7)], true)
}

/// u0 `(tag, value, p)` catches tag 7 from u1 `(tag, value, p)`, whose own `try_call` of
/// u2 catches tag 5 and lands every other one at a cleanup pad that stores the value at
/// `p`, counts itself at `p + 8`, and throws `(tag, value + 1)` again.
pub fn nested(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64, I64], &[I64]), |b, v| {
        let callee = import(b, FUNCS, 1, pair_sig(mk, 3), false);
        let (returned, landings) = try_call(b, callee, None, v, &[Catch::Tag(7)]);
        b.switch_to_block(returned);
        let rs = b.block_params(returned).to_vec();
        let out = mix(b, rs[0], rs[1], 1);
        b.ins().return_(&[out]);
        b.switch_to_block(landings[0]);
        let payload = b.block_params(landings[0]).to_vec();
        let out = mix(b, payload[0], payload[1], 7000);
        b.ins().return_(&[out]);
    });
    let inner = mk.func(1, pair_sig(mk, 3), |b, v| {
        let callee = import(b, FUNCS, 2, pair_sig(mk, 2), false);
        let throw = import(b, HOST, 0, throw_sig(mk), false);
        let (returned, landings) = try_call(b, callee, None, &[v[0], v[1]], &[Catch::Tag(5), Catch::Default]);
        b.switch_to_block(returned);
        let rs = b.block_params(returned).to_vec();
        b.ins().return_(&rs);

        b.switch_to_block(landings[0]);
        let payload = b.block_params(landings[0]).to_vec();
        let k = b.ins().iconst(I64, 500);
        let first = mix(b, payload[0], k, 5);
        b.ins().return_(&[first, payload[1]]);

        b.switch_to_block(landings[1]);
        b.set_cold_block(landings[1]);
        let payload = b.block_params(landings[1]).to_vec();
        b.ins().store(mem(), payload[0], v[2], 0);
        let count = b.ins().load(I64, mem(), v[2], 8);
        let count = b.ins().iadd_imm(count, 1);
        b.ins().store(mem(), count, v[2], 8);
        let bumped = b.ins().iadd_imm(payload[0], 1);
        call(b, throw, &[payload[1], bumped]);
        let zero = b.ins().iconst(I64, 0);
        b.ins().return_(&[zero, zero]);
    });
    let family = Family::new("unwind/nested", vec![entry, inner, thrower(mk, 2)]);
    if mk.windows() { family.either() } else { family }
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use super::super::world::{In, Opts, check, host_mk, i64s};
    use crate::tests::exec::throw_fn;

    /// Each tag with a few values: every tag given has a handler, since an exception no
    /// handler takes ends the process.
    fn inputs(tags: &[i64]) -> Vec<Vec<In>> {
        let mut out = Vec::new();
        for tag in tags {
            for value in [0, 1, -1, 0x1234_5678, i64::MAX] {
                out.push(i64s(&[*tag, value]));
            }
        }
        out
    }

    fn opts<'a>(host: &'a [usize], data: &'a [&'a [u8]]) -> Opts<'a> {
        Opts { data, host, forked: false, interp: false }
    }

    #[test]
    fn a_second_tag_handler_catches_through_both() {
        let Some(mk) = host_mk() else { return };
        let host = [throw_fn()];
        check(&mk, &super::catch_tag(&mk), &inputs(&[0, 5, 7]), &opts(&host, &[]));
    }

    #[test]
    fn a_default_handler_catches_any_tag() {
        let Some(mk) = host_mk() else { return };
        let host = [throw_fn()];
        check(&mk, &super::catch_default(&mk), &inputs(&[0, 5, 7, 9, 1 << 20]), &opts(&host, &[]));
    }

    #[test]
    fn try_call_indirect_catches_through_both() {
        let Some(mk) = host_mk() else { return };
        let host = [throw_fn()];
        check(&mk, &super::catch_indirect(&mk), &inputs(&[0, 5, 7]), &opts(&host, &[]));
    }

    #[test]
    fn a_cleanup_pad_rethrows_to_the_outer_try_call() {
        let Some(mk) = host_mk() else { return };
        let host = [throw_fn()];
        let place = [0u8; 16];
        let mut cases = inputs(&[0, 5, 7]);
        for case in &mut cases {
            case.push(In::Data(0));
        }
        check(&mk, &super::nested(&mk), &cases, &opts(&host, &[&place[..]]));
    }
}
