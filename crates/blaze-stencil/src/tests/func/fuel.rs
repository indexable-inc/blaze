//! Fuel ([`crate::fuel`]): a metered function ticks at its entry and at each loop head, the
//! same number of times on every run and on both backends, and calls the runtime's `out` on
//! exactly the tick after its budget. The runtime here is the host's: `HOST:0` gives the
//! thread's counter, `HOST:1` arms it on the first tick and records a later one.

use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

use cranelift_codegen::{
    ir::{self, InstBuilder, condcodes::IntCC, types},
    settings,
};

use super::{
    Family, HOST, Mk,
    world::{Built, host_mk},
};
use crate::fuel::{self, Metered, Runtime};

const I64: ir::Type = types::I64;
/// What `out` leaves the counter at once the budget is spent: it no longer runs out.
const DISARMED: u64 = u64::MAX / 2;

thread_local! {
    static FUEL: AtomicU64 = const { AtomicU64::new(0) };
    static BUDGET: Cell<u64> = const { Cell::new(0) };
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static OUTS: Cell<u32> = const { Cell::new(0) };
}

extern "C" fn cell() -> *mut u64 {
    FUEL.with(AtomicU64::as_ptr)
}

extern "C" fn out(_cell: *mut u64) {
    FUEL.with(|fuel| {
        if ARMED.replace(true) {
            OUTS.set(OUTS.get() + 1);
            fuel.store(DISARMED, Relaxed);
        } else {
            fuel.store(BUDGET.get() - 1, Relaxed);
        }
    });
}

fn host() -> [usize; 2] {
    [cell as usize, out as usize]
}

fn runtime() -> Runtime {
    Runtime { cell: ir::UserExternalName::new(HOST, 0), out: ir::UserExternalName::new(HOST, 1) }
}

/// `n -> max(n, 0)`, counting `i` up to `n`: an entry, a loop head, a body that jumps back to
/// it, and an exit.
fn counting(mk: &Mk) -> ir::Function {
    mk.func(0, mk.sig_of(&[I64], &[I64]), |b, p| {
        let head = b.create_block();
        let body = b.create_block();
        let exit = b.create_block();
        let i = b.append_block_param(head, I64);
        let zero = b.ins().iconst(I64, 0);
        b.ins().jump(head, &[zero.into()]);
        b.switch_to_block(head);
        let more = b.ins().icmp(IntCC::SignedLessThan, i, p[0]);
        b.ins().brif(more, body, &[], exit, &[]);
        b.switch_to_block(body);
        let next = b.ins().iadd_imm(i, 1);
        b.ins().jump(head, &[next.into()]);
        b.switch_to_block(exit);
        b.ins().return_(&[i]);
    })
}

/// `x -> 2x + 1`: no loop.
fn straight(mk: &Mk) -> ir::Function {
    mk.func(0, mk.sig_of(&[I64], &[I64]), |b, p| {
        let twice = b.ins().iadd(p[0], p[0]);
        let out = b.ins().iadd_imm(twice, 1);
        b.ins().return_(&[out]);
    })
}

fn meter(mk: &Mk, func: fn(&Mk) -> ir::Function) -> (ir::Function, Metered) {
    let mut func = func(mk);
    let metered = fuel::meter(&mut func, &runtime(), mk.cc()).unwrap_or_else(|e| panic!("{e}"));
    let flags = settings::Flags::new(settings::builder());
    if let Err(errors) = cranelift_codegen::verify_function(&func, &flags) {
        panic!("{}", cranelift_codegen::print_errors::pretty_verifier_error(&func, None, errors));
    }
    (func, metered)
}

/// Calls to `HOST:index` in `func`.
fn calls(func: &ir::Function, index: u32) -> usize {
    func.layout
        .blocks()
        .flat_map(|block| func.layout.block_insts(block))
        .filter(|&inst| {
            let ir::InstructionData::Call { func_ref, .. } = func.dfg.insts[inst] else {
                return false;
            };
            let ir::ExternalName::User(name) = func.dfg.ext_funcs[func_ref].name else {
                return false;
            };
            let name = &func.params.user_named_funcs()[name];
            (name.namespace, name.index) == (HOST, index)
        })
        .count()
}

#[test]
fn a_loop_ticks_at_its_entry_and_its_head() {
    let Some(mk) = host_mk() else { return };
    let (func, metered) = meter(&mk, counting);
    assert_eq!(metered, Metered { ticks: 2 }, "{}", func.display());
    assert_eq!(calls(&func, 0), 1, "one counter per call:\n{}", func.display());
    assert_eq!(calls(&func, 1), 2, "one out per tick:\n{}", func.display());
    let (func, metered) = meter(&mk, straight);
    assert_eq!(metered, Metered { ticks: 1 }, "{}", func.display());
}

#[test]
fn metering_gives_the_same_ir_every_time() {
    let Some(mk) = host_mk() else { return };
    let (a, _) = meter(&mk, counting);
    let (b, _) = meter(&mk, counting);
    assert_eq!(a.display().to_string(), b.display().to_string());
}

/// What one call of `counting(n)` under `budget` returned, how many ticks it took, how many
/// times it ran out, and the counter it left.
#[derive(PartialEq, Eq, Debug)]
struct Run {
    result: i64,
    ticks: u64,
    outs: u32,
    left: u64,
}

fn run(exec: &crate::tests::exec::Exec, n: i64, budget: u64) -> Run {
    FUEL.with(|fuel| fuel.store(0, Relaxed));
    BUDGET.set(budget);
    ARMED.set(false);
    OUTS.set(0);
    let result = exec.call::<(i64,), i64>(0, (n,));
    let left = FUEL.with(|fuel| fuel.load(Relaxed));
    let outs = OUTS.get();
    // Ticks 1..=budget spend it (the first arms the thread); tick budget + 1 runs out and
    // disarms, and each one after counts down from `DISARMED`.
    let ticks = if outs == 0 { budget - left } else { budget + 1 + (DISARMED - left) };
    Run { result, ticks, outs, left }
}

#[test]
fn a_loop_stops_at_its_budget_at_the_same_tick_on_every_run() {
    let Some(mk) = host_mk() else { return };
    let (func, _) = meter(&mk, counting);
    let family = Family::new("fuel/counting", vec![func]);
    let built = Built::new(&mk, &family);
    let host = host();
    let mut seen = Vec::new();
    for mix in 0..built.mixes() {
        let exec = built.link(mix, &[], &host);
        let name = built.mix_name(mix);
        // 100 rounds: the entry, then the head 101 times.
        let plenty = run(&exec, 100, 1_000);
        assert_eq!(plenty, Run { result: 100, ticks: 102, outs: 0, left: 1_000 - 102 }, "{name}");
        assert_eq!(run(&exec, 100, 1_000), plenty, "{name}: a second run");
        let short = run(&exec, 100, 10);
        assert_eq!(
            short,
            Run { result: 100, ticks: 102, outs: 1, left: DISARMED - (102 - 11) },
            "{name}"
        );
        assert_eq!(run(&exec, 100, 10), short, "{name}: a second run");
        let exact = run(&exec, 100, 102);
        assert_eq!(exact.outs, 0, "{name}: a budget of exactly the ticks it takes is enough");
        seen.push((plenty, short));
    }
    assert!(seen.windows(2).all(|it| it[0] == it[1]), "both backends tick alike: {seen:?}");
}
