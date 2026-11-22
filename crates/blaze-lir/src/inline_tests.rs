//! The trivial inliner against the evaluator (C1): on random callers and callees (scalar
//! integers, branches, small loops), the inlined function passes `verify` and runs to what the
//! caller does when the evaluator follows its calls; a planted bug (the first spliced call's
//! arguments swapped) is caught. Units pin the bound: an `#[inline(always)]` callee with a loop
//! is spliced, a 17-instruction one is not (and a 16-instruction one is), a cycle ends, and
//! `read` is exactly the spliced bodies, sorted.

use std::alloc::Global;

use blaze_sim::{Broke, Config, Tape, control};

use crate::{
    Flags, IntCond, Op, Ty, Value,
    builder::{BuildError, Builder, Full, Param, Variadic},
    eval::{Bodies, BodyId, Outcome, Val, run_calls_in, run_in},
    func::{BlockKind, Conv, FnAttrs, Func, Inline, RefId, RefKind, SigId, View},
    inline::{Bound, Inlined, TRIVIAL, inline_swapping_first_args, inline_trivial},
    print, verify,
};

/// What the oracle says when the inlined function runs to another outcome.
const DIFFERS: &str = "differs after inlining";

/// Fuel for one run: the random programs' loops run at most three times, nested at most four
/// calls deep.
const FUEL: u64 = 1 << 20;

/// Bodies by id, and which body each body's call reference names.
struct Bank {
    bodies: Vec<(BodyId, Func)>,
    links: Vec<(BodyId, RefId, BodyId)>,
}

impl Bodies for Bank {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let &(_, _, to) = self.links.iter().find(|link| link.0 == from && link.1 == reference)?;
        let (_, func) = self.bodies.iter().find(|body| body.0 == to)?;
        Some((to, func.view()))
    }
}

fn id(n: u8) -> BodyId {
    BodyId([n; 32])
}

/// `(i64, i64) -> i64`, every function's signature here.
fn two_to_one(b: &mut Builder<Global, Full>) -> SigId {
    b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)])
}

/// A function `(x, y) -> body(x, y)`, `#[inline(always)]` if `always`.
fn build(
    always: bool,
    body: impl FnOnce(&mut Builder<Global, Full>, SigId, Value, Value) -> Value,
) -> Result<Func, BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = two_to_one(&mut b);
    b.set_signature(sig);
    if always {
        b.set_attrs(FnAttrs::default().with_inline(Inline::Always));
    }
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let result = body(&mut b, sig, x, y);
    b.ret(&[result]);
    b.finish_checked()
}

/// `acc = start; for _ in 0..trips { acc = op(acc, step) }`, as a loop of two blocks.
fn counted_loop(b: &mut Builder<Global, Full>, op: Op, start: Value, step: Value, trips: u128) -> Value {
    let (head, exit) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    let zero = b.iconst(Ty::I64, 0);
    b.jump((head, &[zero, start]));
    b.begin(head);
    let (i, acc) = (b.param(Ty::I64), b.param(Ty::I64));
    let next = b.binary(op, Flags::NONE, acc, step);
    let one = b.iconst(Ty::I64, 1);
    let i_next = b.binary(Op::Add, Flags::NONE, i, one);
    let limit = b.iconst(Ty::I64, trips);
    let more = b.icmp(IntCond::Ult, i_next, limit);
    b.br(Flags::NONE, more, (head, &[i_next, next]), (exit, &[next]));
    b.begin(exit);
    b.param(Ty::I64)
}

fn calls_left(view: View<'_>) -> usize {
    (0..view.len()).filter(|&at| view.op(at) == Op::Call).count()
}

fn returned(outcome: Outcome<Global>) -> Vec<Val> {
    match outcome {
        Outcome::Returned(values) => values,
        other => panic!("expected a return, got {other:?}"),
    }
}

// The random programs.

const OPS: [Op; 4] = [Op::Add, Op::Sub, Op::Mul, Op::Xor];

struct Gen<'t> {
    t: &'t Tape,
    b: Builder<Global, Full>,
    sig: SigId,
    /// The values usable here: each dominates the current point.
    ints: Vec<Value>,
    /// This function's index and how many there are: it calls only higher ones, so every run
    /// ends.
    me: usize,
    count: usize,
    /// Each call reference made, with the function it names.
    calls: Vec<(RefId, usize)>,
}

impl Gen<'_> {
    fn pick(&self) -> Value {
        self.ints[self.t.index(self.ints.len(), "operand")]
    }

    fn statement(&mut self) {
        match self.t.below(5, "statement") {
            0 => {
                let (x, y) = (self.pick(), self.pick());
                let op = OPS[self.t.index(OPS.len(), "op")];
                let made = self.b.binary(op, Flags::NONE, x, y);
                self.ints.push(made);
            }
            2 if self.me + 1 < self.count => {
                let to = self.me + 1 + self.t.index(self.count - self.me - 1, "callee");
                let reference = self.b.reference(RefKind::Func, to as u32);
                self.calls.push((reference, to));
                let (x, y) = (self.pick(), self.pick());
                let made = self.b.call(Flags::NONE, reference, self.sig, &[x, y]);
                self.ints.push(made);
            }
            3 => {
                // A diamond: each arm one operation of values from before it; the join's
                // parameter is what comes out.
                let (x, y) = (self.pick(), self.pick());
                let cond = self.b.icmp(IntCond::Slt, x, y);
                let then = self.b.create_block(BlockKind::Normal);
                let other = self.b.create_block(BlockKind::Normal);
                let join = self.b.create_block(BlockKind::Normal);
                self.b.br(Flags::NONE, cond, (then, &[]), (other, &[]));
                self.b.begin(then);
                let (p, q) = (self.pick(), self.pick());
                let left = self.b.binary(Op::Add, Flags::NONE, p, q);
                self.b.jump((join, &[left]));
                self.b.begin(other);
                let (p, q) = (self.pick(), self.pick());
                let right = self.b.binary(Op::Sub, Flags::NONE, p, q);
                self.b.jump((join, &[right]));
                self.b.begin(join);
                let joined = self.b.param(Ty::I64);
                self.ints.push(joined);
            }
            4 => {
                let (start, step) = (self.pick(), self.pick());
                let op = OPS[self.t.index(OPS.len(), "loop op")];
                let trips = 1 + u128::from(self.t.below(3, "trips"));
                let made = counted_loop(&mut self.b, op, start, step, trips);
                self.ints.push(made);
            }
            _ => {
                let made = self.b.iconst(Ty::I64, u128::from(self.t.below(16, "constant")));
                self.ints.push(made);
            }
        }
    }
}

/// Function `me` of `count`, and its calls.
fn random_func(t: &Tape, me: usize, count: usize) -> Result<(Func, Vec<(RefId, usize)>), BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = two_to_one(&mut b);
    b.set_signature(sig);
    if t.one_in(3, "always") {
        b.set_attrs(FnAttrs::default().with_inline(Inline::Always));
    }
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let ints = vec![b.param(Ty::I64), b.param(Ty::I64)];
    let mut g = Gen { t, b, sig, ints, me, count, calls: Vec::new() };
    for _ in 0..t.below(7, "statements") {
        g.statement();
    }
    let result = g.pick();
    g.b.ret(&[result]);
    let func = g.b.finish_checked()?;
    Ok((func, g.calls))
}

/// Two to four functions; function 0 (id 1) is the root.
fn random_bank(t: &Tape) -> Result<Bank, BuildError> {
    let count = 2 + t.index(3, "functions");
    let mut bank = Bank { bodies: Vec::new(), links: Vec::new() };
    for me in 0..count {
        let (func, calls) = random_func(t, me, count)?;
        for (reference, to) in calls {
            bank.links.push((id(me as u8 + 1), reference, id(to as u8 + 1)));
        }
        bank.bodies.push((id(me as u8 + 1), func));
    }
    Ok(bank)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The first spliced call's first two arguments swapped.
    SwapArgs,
}

/// The oracle: the output is LIR, `read` is sorted and unique, and the output runs (by
/// `run_in` when no call is left, else following the root's remaining calls) to what the
/// caller runs to with its calls followed.
fn inline_agrees(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let bank = random_bank(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let (root_id, root) = (bank.bodies[0].0, bank.bodies[0].1.view());
    let args = [Val::Int(u128::from(t.below(1 << 20, "x"))), Val::Int(u128::from(t.below(1 << 20, "y")))];
    let before = run_calls_in(root, root_id, &args, FUEL, &bank, Global)
        .map_err(|e| Broke::Harness(format!("the caller does not run: {e:?}\n{}", print(root))))?;
    if !matches!(before, Outcome::Returned(_) | Outcome::Trapped(_)) {
        return Err(Broke::Harness(format!("the caller ends as {before:?}\n{}", print(root))));
    }
    let made = match plant {
        Plant::None => inline_trivial(root, root_id, &bank, TRIVIAL, Global),
        Plant::SwapArgs => inline_swapping_first_args(root, root_id, &bank, TRIVIAL, Global),
    };
    let Inlined { func, read } = made.map_err(|e| Broke::Oracle(format!("the inliner failed: {e}\n{}", print(root))))?;
    let out = func.view();
    verify(out).map_err(|e| Broke::Oracle(format!("the output is not LIR: {e}\n{}", print(out))))?;
    if read.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Broke::Oracle(format!("`read` is not sorted and unique: {read:?}")));
    }
    let after = match calls_left(out) {
        0 => run_in(out, &args, FUEL, Global),
        _ => run_calls_in(out, root_id, &args, FUEL, &bank, Global),
    }
    .map_err(|e| Broke::Oracle(format!("{DIFFERS}: the output does not run: {e:?}\n{}", print(out))))?;
    if after != before {
        return Err(Broke::Oracle(format!(
            "{DIFFERS}: {before:?} became {after:?}\nbefore:\n{}\nafter:\n{}",
            print(root),
            print(out)
        )));
    }
    Ok(())
}

/// Random callers and callees agree with the evaluator before and after inlining, and the
/// control catches the planted argument swap. Expected on stderr: "plant SwapArgs: caught at
/// case k of 512".
#[test]
fn inlining_keeps_what_the_caller_computes() {
    control(Config::cases(512), Plant::None, Plant::SwapArgs, DIFFERS, inline_agrees);
}

/// An `#[inline(always)]` callee with a loop is spliced: the bound's size and acyclicity yield
/// to the attribute (backend-next.md, C2). `g(x, y)` adds `y` to `x` three times, so
/// `f(5, 7) = g(5, 7) = 26`. Expected: passes, `read == [g]`, no call left.
#[test]
fn an_always_callee_with_a_loop_is_spliced() {
    let g = build(true, |b, _, x, y| counted_loop(b, Op::Add, x, y, 3)).expect("g builds");
    let f = build(false, |b, sig, x, y| {
        let reference = b.reference(RefKind::Func, 0);
        b.call(Flags::NONE, reference, sig, &[x, y])
    })
    .expect("f builds");
    let bank = Bank { bodies: vec![(id(1), f), (id(2), g)], links: vec![(id(1), RefId(0), id(2))] };
    let root = bank.bodies[0].1.view();
    let inlined = inline_trivial(root, id(1), &bank, TRIVIAL, Global).expect("f inlines");
    assert_eq!(inlined.read[..], [id(2)]);
    let out = inlined.func.view();
    assert_eq!(calls_left(out), 0, "no call is left:\n{}", print(out));
    let ran = run_in(out, &[Val::Int(5), Val::Int(7)], FUEL, Global).expect("the output runs");
    assert_eq!(returned(ran)[..], [Val::Int(26)]);
}

/// `x + y + y + ..` with `adds` additions: `2 + adds + 1` instructions (two parameters, the
/// additions, the return).
fn chain(adds: usize) -> Func {
    build(false, |b, _, x, y| {
        let mut acc = x;
        for _ in 0..adds {
            acc = b.binary(Op::Add, Flags::NONE, acc, y);
        }
        acc
    })
    .expect("the chain builds")
}

fn calling_place_zero() -> Func {
    build(false, |b, sig, x, y| {
        let reference = b.reference(RefKind::Func, 0);
        b.call(Flags::NONE, reference, sig, &[x, y])
    })
    .expect("the caller builds")
}

/// `TRIVIAL.insts` is 16: a loop-free callee of 17 instructions is not spliced, one of 16 is.
/// Expected: passes; the first `read` empty with its call left, the second `read == [g]`.
#[test]
fn a_seventeen_instruction_callee_is_not_spliced() {
    let big = chain(14);
    assert_eq!(big.view().len(), 17, "the callee has 17 instructions");
    let bank = Bank { bodies: vec![(id(1), calling_place_zero()), (id(2), big)], links: vec![(id(1), RefId(0), id(2))] };
    let inlined = inline_trivial(bank.bodies[0].1.view(), id(1), &bank, TRIVIAL, Global).expect("f inlines");
    assert!(inlined.read.is_empty(), "nothing spliced: {:?}", inlined.read);
    assert_eq!(calls_left(inlined.func.view()), 1);

    let small = chain(13);
    assert_eq!(small.view().len(), 16, "the callee has 16 instructions");
    let bank = Bank { bodies: vec![(id(1), calling_place_zero()), (id(2), small)], links: vec![(id(1), RefId(0), id(2))] };
    let inlined = inline_trivial(bank.bodies[0].1.view(), id(1), &bank, TRIVIAL, Global).expect("f inlines");
    assert_eq!(inlined.read[..], [id(2)]);
    assert_eq!(calls_left(inlined.func.view()), 0);
}

/// `f -> g -> f`, both `#[inline(always)]`: the inliner ends. `f` is on the inlining stack, so
/// `g`'s call to it stays, which `f`'s references cannot name, so `g` is not spliced either.
/// Expected: passes, `read` empty, `f`'s one call left, the output verified.
#[test]
fn a_cycle_ends() {
    let f = build(true, |b, sig, x, y| {
        let reference = b.reference(RefKind::Func, 0);
        b.call(Flags::NONE, reference, sig, &[x, y])
    })
    .expect("f builds");
    let g = build(true, |b, sig, x, y| {
        let reference = b.reference(RefKind::Func, 0);
        b.call(Flags::NONE, reference, sig, &[y, x])
    })
    .expect("g builds");
    let bank = Bank {
        bodies: vec![(id(1), f), (id(2), g)],
        links: vec![(id(1), RefId(0), id(2)), (id(2), RefId(0), id(1))],
    };
    let inlined = inline_trivial(bank.bodies[0].1.view(), id(1), &bank, TRIVIAL, Global).expect("f inlines");
    assert!(inlined.read.is_empty(), "nothing spliced: {:?}", inlined.read);
    let out = inlined.func.view();
    verify(out).expect("the output is LIR");
    assert_eq!(calls_left(out), 1);
}

/// `read` is exactly the spliced bodies, sorted by id and unique: `f` calls `g` twice and `h`
/// once, `g` calls `k`, `h` has 17 instructions. `g` (id 9) and `k` (id 3) are spliced, `g`
/// twice; `h` (id 5) is not. Expected: passes, `read == [k, g]`, `h`'s one call left, and the
/// output computes what `f` does.
#[test]
fn read_is_exactly_the_spliced_bodies() {
    let f = build(false, |b, sig, x, y| {
        let (g, h) = (b.reference(RefKind::Func, 0), b.reference(RefKind::Func, 1));
        let first = b.call(Flags::NONE, g, sig, &[x, y]);
        let second = b.call(Flags::NONE, g, sig, &[first, x]);
        b.call(Flags::NONE, h, sig, &[second, y])
    })
    .expect("f builds");
    let g = build(false, |b, sig, x, y| {
        let k = b.reference(RefKind::Func, 0);
        let made = b.call(Flags::NONE, k, sig, &[x, y]);
        b.binary(Op::Mul, Flags::NONE, made, y)
    })
    .expect("g builds");
    let k = build(false, |b, _, x, y| b.binary(Op::Sub, Flags::NONE, x, y)).expect("k builds");
    let bank = Bank {
        bodies: vec![(id(1), f), (id(9), g), (id(3), k), (id(5), chain(14))],
        links: vec![(id(1), RefId(0), id(9)), (id(1), RefId(1), id(5)), (id(9), RefId(0), id(3))],
    };
    let root = bank.bodies[0].1.view();
    let inlined = inline_trivial(root, id(1), &bank, TRIVIAL, Global).expect("f inlines");
    assert_eq!(inlined.read[..], [id(3), id(9)]);
    let out = inlined.func.view();
    assert_eq!(calls_left(out), 1, "only the call of h is left:\n{}", print(out));
    let args = [Val::Int(11), Val::Int(4)];
    let before = run_calls_in(root, id(1), &args, FUEL, &bank, Global).expect("f runs");
    let after = run_calls_in(out, id(1), &args, FUEL, &bank, Global).expect("the output runs");
    assert_eq!(after, before);
}

/// The bound is a plain value: a zero growth splices nothing. Expected: passes, `read` empty.
#[test]
fn no_growth_splices_nothing() {
    let bank = Bank { bodies: vec![(id(1), calling_place_zero()), (id(2), chain(1))], links: vec![(id(1), RefId(0), id(2))] };
    let bound = Bound { growth: 0, ..TRIVIAL };
    let inlined = inline_trivial(bank.bodies[0].1.view(), id(1), &bank, bound, Global).expect("f inlines");
    assert!(inlined.read.is_empty());
    assert_eq!(calls_left(inlined.func.view()), 1);
}

#[test]
fn fn_summary_and_guided_pgo_priority_and_external_refs() {
    use crate::inline::{GuidedInlined, InlinePolicy, NoWeights, SiteWeights, WithImported, inline_guided};
    use crate::summary::FnSummary;

    // 1. FnSummary deterministic hash & loop detection.
    let pure = chain(4);
    let s1 = FnSummary::compute(pure.view(), &Global);
    let s2 = FnSummary::compute(pure.view(), &Global);
    assert_eq!(s1, s2);
    assert!(!s1.has_loops);
    assert!(s1.is_closed);
    assert!(s1.has_return);

    let looped = build(false, |b, _, x, y| counted_loop(b, Op::Add, x, y, 3)).expect("looped builds");
    let sl = FnSummary::compute(looped.view(), &Global);
    assert!(sl.has_loops);
    assert_ne!(s1.summary_hash, sl.summary_hash);

    // 2. PGO hot-site priority under tight growth budget: caller calls g (site A) then h (site B).
    // Both g and h have 12 insts (chain(9)), growth budget = 14 fits only ONE; PGO marks site B hot.
    let g = chain(9);
    let h = chain(9);
    let caller = build(false, |b, sig, x, y| {
        let rg = b.reference(RefKind::Func, 0);
        let rh = b.reference(RefKind::Func, 1);
        let a = b.call(Flags::NONE, rg, sig, &[x, y]);
        b.call(Flags::NONE, rh, sig, &[a, y])
    })
    .expect("caller builds");
    // Find the two call instruction indices in `caller`.
    let call_sites: Vec<u32> = (0..caller.view().len())
        .filter(|&i| caller.view().op(i) == Op::Call)
        .map(|i| i as u32)
        .collect();
    assert_eq!(call_sites.len(), 2);

    struct HotSecond(u32);
    impl SiteWeights for HotSecond {
        fn weight(&self, _caller: BodyId, site_inst: u32) -> u64 {
            if site_inst == self.0 { 1000 } else { 1 }
        }
    }

    let bank = Bank {
        bodies: vec![(id(1), caller), (id(2), g), (id(3), h)],
        links: vec![(id(1), RefId(0), id(2)), (id(1), RefId(1), id(3))],
    };
    let root = bank.bodies[0].1.view();
    let policy = InlinePolicy { max_growth: 14, ..InlinePolicy::default() };
    let GuidedInlined { func, read, .. } =
        inline_guided(root, id(1), &bank, &HotSecond(call_sites[1]), policy, Global).expect("guided inlines");
    // Hot second call (`id(3)`) was prioritized over cold first call (`id(2)`).
    assert_eq!(read[..], [id(3)], "expected hot callee id(3) to win tight budget, got {read:?}");
    let args = [Val::Int(5), Val::Int(3)];
    let before = run_calls_in(root, id(1), &args, FUEL, &bank, Global).expect("before runs");
    let after = run_calls_in(func.view(), id(1), &args, FUEL, &bank, Global).expect("after runs");
    assert_eq!(after, before);

    // 3. Lifting CLOSED barrier: mid calls un-inlinable leaf (missing from bank or too large),
    // yet mid itself inlines into main and imports the external Func RefRec cleanly.
    let huge = chain(100);
    let mid = build(false, |b, sig, x, y| {
        let rleaf = b.reference(RefKind::Func, 0);
        let v = b.call(Flags::NONE, rleaf, sig, &[x, y]);
        b.binary(Op::Add, Flags::NONE, v, y)
    })
    .expect("mid builds");
    let main_fn = calling_place_zero();
    let bank2 = Bank {
        bodies: vec![(id(1), main_fn), (id(2), mid), (id(3), huge)],
        links: vec![(id(1), RefId(0), id(2)), (id(2), RefId(0), id(3))],
    };
    let root2 = bank2.bodies[0].1.view();
    let GuidedInlined { func: out2, read: read2, imported_refs } =
        inline_guided(root2, id(1), &bank2, &NoWeights, InlinePolicy { max_callee_insts: 32, allow_external_refs: true, ..InlinePolicy::default() }, Global)
            .expect("open callee inlines");
    assert_eq!(read2[..], [id(2)], "mid should inline even though it calls un-inlined huge");
    assert!(!imported_refs.is_empty(), "external ref to huge should be imported");
    let adapted = WithImported { root: id(1), imported: &imported_refs, inner: &bank2 };
    let before2 = run_calls_in(root2, id(1), &args, FUEL, &bank2, Global).expect("before2 runs");
    let after2 = run_calls_in(out2.view(), id(1), &args, FUEL, &adapted, Global).expect("after2 runs");
    assert_eq!(after2, before2);
}

#[test]
fn lto_bottom_up_scc_and_jit_tier_up() {
    use blaze_own::Vec as OwnVec;
    use crate::{
        inline::InlinePolicy,
        lto::{ModuleBody, advise_jit_tier_up, lto_optimize_module, tarjan_sccs},
        pgo::{BodyWeight, PgoSiteTable, SiteEntry},
    };

    // 1. Tarjan SCC: 0 -> 1 -> {2 <-> 3}. Callee SCC {2, 3} comes first, then {1}, then {0}.
    let adj_offsets = [0u32, 1, 2, 3, 4];
    let adj = [1u32, 2, 3, 2];
    let sccs = tarjan_sccs(4, &adj_offsets, &adj, Global);
    assert_eq!(sccs.len(), 3);
    assert_eq!(sccs.component(0), &[2, 3]);
    assert_eq!(sccs.component(1), &[1]);
    assert_eq!(sccs.component(2), &[0]);

    // 2. End-to-end 3-function chain `main (id 1) -> mid (id 2) -> leaf (id 3)` with PGO weights:
    // leaf(x, y) = (x + y) * 3; mid(x, y) = leaf(x, y) + 7; main(x, y) = mid(x, y) ^ y.
    let leaf = build(false, |b, _sig, x, y| {
        let sum = b.binary(Op::Add, Flags::NONE, x, y);
        let three = b.iconst(Ty::I64, 3);
        b.binary(Op::Mul, Flags::NONE, sum, three)
    })
    .expect("leaf");
    let mid = build(false, |b, sig, x, y| {
        let r = b.reference(RefKind::Func, 0);
        let v = b.call(Flags::NONE, r, sig, &[x, y]);
        let seven = b.iconst(Ty::I64, 7);
        b.binary(Op::Add, Flags::NONE, v, seven)
    })
    .expect("mid");
    let main_fn = build(false, |b, sig, x, y| {
        let r = b.reference(RefKind::Func, 0);
        let v = b.call(Flags::NONE, r, sig, &[x, y]);
        b.binary(Op::Xor, Flags::NONE, v, y)
    })
    .expect("main");

    // Also include a hot looping function `hot_loop (id 4)` to test JIT tier-up priority.
    let hot_loop = build(false, |b, _sig, x, y| counted_loop(b, Op::Add, x, y, 8)).expect("loop");

    // Baseline evaluation before LTO.
    let ref_bank = Bank {
        bodies: vec![
            (id(1), build(false, |b, sig, x, y| {
                let r = b.reference(RefKind::Func, 0);
                let v = b.call(Flags::NONE, r, sig, &[x, y]);
                b.binary(Op::Xor, Flags::NONE, v, y)
            }).unwrap()),
            (id(2), build(false, |b, sig, x, y| {
                let r = b.reference(RefKind::Func, 0);
                let v = b.call(Flags::NONE, r, sig, &[x, y]);
                let seven = b.iconst(Ty::I64, 7);
                b.binary(Op::Add, Flags::NONE, v, seven)
            }).unwrap()),
            (id(3), build(false, |b, _sig, x, y| {
                let sum = b.binary(Op::Add, Flags::NONE, x, y);
                let three = b.iconst(Ty::I64, 3);
                b.binary(Op::Mul, Flags::NONE, sum, three)
            }).unwrap()),
            (id(4), build(false, |b, _sig, x, y| counted_loop(b, Op::Add, x, y, 8)).unwrap()),
        ],
        links: vec![(id(1), RefId(0), id(2)), (id(2), RefId(0), id(3))],
    };

    let main_call = (0..main_fn.view().len()).find(|&i| main_fn.view().op(i) == Op::Call).unwrap() as u32;
    let mid_call = (0..mid.view().len()).find(|&i| mid.view().op(i) == Op::Call).unwrap() as u32;
    let pgo = PgoSiteTable::new_in(
        &[
            SiteEntry { caller: id(1), site_inst: main_call, weight: 500 },
            SiteEntry { caller: id(2), site_inst: mid_call, weight: 500 },
        ],
        &[
            BodyWeight { body: id(1), weight: 500 },
            BodyWeight { body: id(4), weight: 300 },
            BodyWeight { body: id(3), weight: 5 },
        ],
        1,
        Global,
    );

    let mut module = OwnVec::new_in(Global);
    module.push(ModuleBody { id: id(1), func: main_fn });
    module.push(ModuleBody { id: id(2), func: mid });
    module.push(ModuleBody { id: id(3), func: leaf });
    module.push(ModuleBody { id: id(4), func: hot_loop });

    let lto = lto_optimize_module(module, &ref_bank, &pgo, InlinePolicy::default(), Global)
        .expect("lto succeeds");
    assert_eq!(lto.scc_count, 4);
    assert!(lto.total_inlined_bodies >= 2, "expected leaf->mid and mid->main inlining");

    // Verify optimized `main` has 0 remaining calls and evaluates bit-identically across inputs!
    let opt_main = &lto.bodies[0];
    assert!(verify(opt_main.func.view()).is_ok());
    assert_eq!(calls_left(opt_main.func.view()), 0, "main should have collapsed all calls bottom-up");
    assert!(opt_main.summary_changed);

    for (a, b) in [(0u128, 0u128), (5, 3), (17, 42), (1000, 999), (u64::MAX as u128, 7)] {
        let args = [Val::Int(a), Val::Int(b)];
        let expected = run_calls_in(ref_bank.bodies[0].1.view(), id(1), &args, FUEL, &ref_bank, Global)
            .expect("baseline runs");
        let got = run_in(opt_main.func.view(), &args, FUEL, Global).expect("opt main runs");
        assert_eq!(returned(got), returned(expected), "mismatch for ({a}, {b})");
    }

    // 3. JIT tier-up planner: `hot_loop` (weight 300 * 4x loop boost = 1200) ranks above `main` (500)
    // and `mid` (500 via call-site sum), while `leaf` (weight 5 < threshold 100) is excluded.
    let tier_up = advise_jit_tier_up(&lto.bodies, &pgo, 100, Global);
    assert_eq!(tier_up.len(), 3);
    assert_eq!(tier_up[0].body, id(4));
    assert!(tier_up[0].has_loops);
    assert_eq!(tier_up[0].priority, 1200);
    assert_eq!(tier_up[1].priority, 500);
    assert_eq!(tier_up[2].priority, 500);
    assert!(!tier_up.iter().any(|c| c.body == id(3)), "cold leaf must be excluded");

    // 4. Cross-run BPG1 encode/decode round-trip + corruption refusal + exponential decay + static cold-start estimate.
    let mut encoded = pgo.encode(Global);
    let decoded = PgoSiteTable::decode(&encoded, Global).expect("valid BPG1 decodes");
    assert_eq!(decoded.site_entries(), pgo.site_entries());
    assert_eq!(decoded.body_entries(), pgo.body_entries());
    assert_eq!(decoded.tier_of(id(1)), crate::pgo::PgoTier::Hot);
    assert_eq!(decoded.tier_of(id(3)), crate::pgo::PgoTier::Warm);
    assert_eq!(decoded.tier_of(id(99)), crate::pgo::PgoTier::Cold);
    // Single-bit corruption is refused by BLAKE3 digest check.
    encoded[10] ^= 0x01;
    assert_eq!(PgoSiteTable::decode(&encoded, Global).err(), Some(crate::pgo::PgoRecordError::BadDigest));

    // Decayed cross-run merge: 50% of old (500 -> 250) + fresh (100) = 350.
    let fresh = PgoSiteTable::new_in(&[], &[BodyWeight { body: id(1), weight: 100 }], 1, Global);
    let merged = pgo.merge_decayed(&fresh, 1, 2, Global);
    assert_eq!(merged.body_weight(id(1)), 350);

    // Cold-start static estimate assigns higher weight to looping bodies than straight-line bodies.
    let mut est_mod = OwnVec::new_in(Global);
    est_mod.push(ModuleBody { id: id(10), func: build(false, |b, _sig, x, y| b.binary(Op::Add, Flags::NONE, x, y)).unwrap() });
    est_mod.push(ModuleBody { id: id(11), func: build(false, |b, _sig, x, y| counted_loop(b, Op::Add, x, y, 4)).unwrap() });
    let est = PgoSiteTable::estimate_static(&est_mod, Global);
    assert!(est.body_weight(id(11)) > est.body_weight(id(10)));
    assert_eq!(est.tier_of(id(11)), crate::pgo::PgoTier::Hot);
}
