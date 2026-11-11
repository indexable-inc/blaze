//! The refinement behind spending facts (docs/v2/plan/design/facts.md), on a language small
//! enough to run every case: functions that print, panic or hang on their argument and call
//! one another, directly or through an object, each call with an optional cleanup (printed
//! when the call unwinds), an optional guard and a result that is used or not.
//!
//! What is real here and what is the model's. Real: which facts a set of effect kinds proves
//! ([`inferred_with`]), which facts each change needs and what it records
//! ([`Facts::license_with`]), and which callees are asked at all ([`Callee::asked`]): the
//! code `fn_facts` and hir-ty-mir's `SpendFacts` run. The model's: the language, its
//! evaluator, a summary that counts a call through an object as the walk does (as
//! [`Rule::Indirect`], the object's own effects counted where it is made, not at the call),
//! and an optimizer that makes every [`Spend`], the two MIR does not make yet included.

use hegel::{TestCase, generators as gs};
use hir_ty::facts::{Callee, Fact, Facts, Plant as SpendPlant, Spend};

use super::{Plant as ProvePlant, UNSEEN, inferred_with};
use crate::effects::{EffectSet, PURE, Rule};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arg {
    /// The caller's own argument.
    Own,
    Const(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The function at this index, by name.
    Direct(usize),
    /// Whatever function the object in this slot carries.
    Object(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Call {
    target: Target,
    arg: Arg,
    /// Printed when the call unwinds, before the unwinding goes on: the cleanup its unwind
    /// edge leads to.
    cleanup: Option<u8>,
    /// The call runs only when the caller's argument is this.
    guard: Option<u8>,
    /// The result is printed.
    used: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Print(u8),
    /// Panics when the function's argument is this.
    PanicIf(u8),
    /// Runs for ever when the function's argument is this.
    HangIf(u8),
    Call(Call),
    /// Prints the result of the call before it: what a merged repeat is left as.
    PrintLast,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Func {
    ops: Vec<Op>,
    /// Added to the argument to give the result.
    adds: u8,
}

/// Functions call only later ones, so no call recurs; `objects[slot]` is the function the
/// object in that slot carries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Program {
    funcs: Vec<Func>,
    objects: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Returned(u8),
    Panicked,
    /// Ran until the fuel was gone.
    Hung,
}

/// What a run is seen to do: what it printed, in order, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    out: Vec<u8>,
    end: End,
}

/// More than any terminating run of a drawn program takes (five functions of four ops, each
/// call one level down: at most 4 + 4^2 + .. + 4^5 steps).
const FUEL: u32 = 4096;

impl Program {
    fn callee(&self, target: Target) -> usize {
        match target {
            Target::Direct(at) => at,
            Target::Object(slot) => self.objects[slot],
        }
    }

    fn run(&self, arg: u8) -> Seen {
        let (mut out, mut fuel) = (Vec::new(), FUEL);
        let end = self.call(0, arg, &mut out, &mut fuel);
        Seen { out, end }
    }

    fn call(&self, at: usize, arg: u8, out: &mut Vec<u8>, fuel: &mut u32) -> End {
        let mut last = 0;
        for op in &self.funcs[at].ops {
            if *fuel == 0 {
                return End::Hung;
            }
            *fuel -= 1;
            match *op {
                Op::Print(what) => out.push(what),
                Op::PanicIf(when) if when == arg => return End::Panicked,
                Op::HangIf(when) if when == arg => {
                    *fuel = 0;
                    return End::Hung;
                }
                Op::PanicIf(_) | Op::HangIf(_) => {}
                Op::PrintLast => out.push(last),
                Op::Call(call) => {
                    if call.guard.is_some_and(|it| it != arg) {
                        continue;
                    }
                    let given = match call.arg {
                        Arg::Own => arg,
                        Arg::Const(it) => it,
                    };
                    match self.call(self.callee(call.target), given, out, fuel) {
                        End::Returned(result) => {
                            last = result;
                            if call.used {
                                out.push(result);
                            }
                        }
                        End::Panicked => {
                            out.extend(call.cleanup);
                            return End::Panicked;
                        }
                        End::Hung => return End::Hung,
                    }
                }
            }
        }
        End::Returned(arg.wrapping_add(self.funcs[at].adds))
    }

    /// Each function's kinds of effect, as the walk's summary has them: its own, and those
    /// of what it calls by name. A call through an object counts as [`Rule::Indirect`] and
    /// nothing else: what the object's function does is counted where the object is made.
    fn kinds(&self) -> Vec<EffectSet> {
        let mut kinds = vec![EffectSet::EMPTY; self.funcs.len()];
        for at in (0..self.funcs.len()).rev() {
            let mut own = EffectSet::EMPTY;
            for op in &self.funcs[at].ops {
                own = own.union(match *op {
                    Op::Print(_) | Op::PrintLast => EffectSet::one(Rule::Io),
                    Op::PanicIf(_) => EffectSet::one(Rule::Panic),
                    Op::HangIf(_) => EffectSet::one(Rule::Loop),
                    Op::Call(Call { target: Target::Direct(callee), .. }) => kinds[callee],
                    Op::Call(Call { target: Target::Object(_), .. }) => {
                        EffectSet::one(Rule::Indirect)
                    }
                });
            }
            kinds[at] = own;
        }
        kinds
    }

    /// Each function's record, as `fn_facts` makes it from its kinds.
    fn facts(&self, plant: ProvePlant) -> Vec<Facts> {
        let kinds = self.kinds();
        (0..self.funcs.len())
            .map(|at| {
                let mut inferred = inferred_with(kinds[at], plant);
                if kinds[at].intersect(PURE.refuses.union(UNSEEN)).is_empty() {
                    inferred = inferred.with(Fact::Pure);
                }
                Facts { inferred, ..Facts::none([at as u8; 32]) }
            })
            .collect()
    }
}

/// The bugs planted in one run of the optimizer.
#[derive(Clone, Copy)]
struct Plants {
    prove: ProvePlant,
    spend: SpendPlant,
}

const NONE: Plants = Plants { prove: ProvePlant::Off, spend: SpendPlant::Off };

/// A function's code after the passes, and what they recorded of other functions' facts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Optimized {
    ops: Vec<Op>,
    relied: Vec<[u8; 32]>,
}

impl Optimized {
    fn rely(&mut self, digests: impl IntoIterator<Item = [u8; 32]>) {
        for digest in digests {
            if !self.relied.contains(&digest) {
                self.relied.push(digest);
            }
        }
    }
}

/// How the site names its callee: the one question a pass asks before any fact.
fn named(target: Target) -> (Callee, Option<usize>) {
    match target {
        Target::Direct(at) => (Callee::Direct, Some(at)),
        Target::Object(_) => (Callee::Dyn, None),
    }
}

/// The passes over one function: every [`Spend`], each through [`Facts::license_with`].
fn optimize(func: &Func, facts: &[Facts], plants: Plants) -> Optimized {
    let mut done = Optimized { ops: func.ops.clone(), relied: Vec::new() };
    let license = |call: &Call, spend: Spend| {
        let (how, callee) = named(call.target);
        if !how.asked() {
            return None;
        }
        facts[callee?].license_with(spend, plants.spend)
    };
    // The unwind edge of a call of a function that cannot unwind.
    for at in 0..done.ops.len() {
        let Op::Call(mut call) = done.ops[at] else { continue };
        if call.cleanup.is_none() {
            continue;
        }
        let Some(relied) = license(&call, Spend::UnwindEdge) else { continue };
        call.cleanup = None;
        done.ops[at] = Op::Call(call);
        done.rely(relied);
    }
    // A guarded call whose result nothing reads, above its guard.
    for at in 0..done.ops.len() {
        let Op::Call(mut call) = done.ops[at] else { continue };
        if call.guard.is_none() || call.used {
            continue;
        }
        let Some(relied) = license(&call, Spend::Hoist) else { continue };
        call.guard = None;
        done.ops[at] = Op::Call(call);
        done.rely(relied);
    }
    // The second of two calls in a row of one function with one argument.
    let mut at = 1;
    while at < done.ops.len() {
        let (Op::Call(first), Op::Call(second)) = (done.ops[at - 1], done.ops[at]) else {
            at += 1;
            continue;
        };
        let same = first.target == second.target
            && first.arg == second.arg
            && first.guard.is_none()
            && second.guard.is_none();
        let Some(relied) = same.then(|| license(&second, Spend::MergeRepeat)).flatten() else {
            at += 1;
            continue;
        };
        done.rely(relied);
        if second.used {
            done.ops[at] = Op::PrintLast;
            at += 1;
        } else {
            // Nothing reads it: the first call stays the op before whatever follows.
            done.ops.remove(at);
        }
    }
    // A call whose result nothing reads.
    let mut at = 0;
    while at < done.ops.len() {
        let read = match done.ops[at] {
            Op::Call(call) => call.used || matches!(done.ops.get(at + 1), Some(Op::PrintLast)),
            _ => true,
        };
        let Op::Call(call) = done.ops[at] else {
            at += 1;
            continue;
        };
        match (!read).then(|| license(&call, Spend::DeadCall)).flatten() {
            Some(relied) => {
                done.rely(relied);
                done.ops.remove(at);
            }
            None => at += 1,
        }
    }
    done
}

/// The program with every function optimized.
fn optimized(program: &Program, plants: Plants) -> (Program, Vec<Optimized>) {
    let facts = program.facts(plants.prove);
    let done: Vec<Optimized> =
        program.funcs.iter().map(|func| optimize(func, &facts, plants)).collect();
    let funcs = std::iter::zip(&program.funcs, &done)
        .map(|(func, done)| Func { ops: done.ops.clone(), adds: func.adds })
        .collect();
    (Program { funcs, objects: program.objects.clone() }, done)
}

/// The oracle of the refinement: the optimized program is seen to do, for every argument,
/// what the program as written does.
fn refines(program: &Program, plants: Plants) -> Result<(), String> {
    let (after, _) = optimized(program, plants);
    for arg in 0..4 {
        let (want, got) = (program.run(arg), after.run(arg));
        if want != got {
            return Err(format!(
                "argument {arg}: as written {want:?}, optimized {got:?}\n{program:#?}\n{after:#?}"
            ));
        }
    }
    Ok(())
}

/// The oracle of the keys: a function's key is its own code as written and what its passes
/// recorded; two programs that give a function one key give it one optimized code.
fn covered(a: &Program, b: &Program, plants: Plants) -> Result<(), String> {
    let (_, in_a) = optimized(a, plants);
    let (_, in_b) = optimized(b, plants);
    for at in 0..a.funcs.len().min(b.funcs.len()) {
        let same_key = a.funcs[at] == b.funcs[at] && in_a[at].relied == in_b[at].relied;
        if same_key && in_a[at].ops != in_b[at].ops {
            return Err(format!(
                "function {at} has one key and two codes: {:?} and {:?}",
                in_a[at].ops, in_b[at].ops
            ));
        }
    }
    Ok(())
}

fn below(tc: &TestCase, n: u32) -> u32 {
    tc.draw(gs::integers::<u32>().max_value(n - 1))
}

fn maybe(tc: &TestCase, what: u8) -> Option<u8> {
    (below(tc, 2) == 0).then_some(what)
}

fn drawn_op(tc: &TestCase, at: usize, funcs: usize, objects: &[usize]) -> Op {
    let small = below(tc, 3) as u8;
    match below(tc, 6) {
        0 => Op::Print(small),
        1 => Op::PanicIf(small),
        2 => Op::HangIf(small),
        kind => {
            // Only later functions: no call recurs.
            let later: Vec<usize> = (at + 1..funcs).collect();
            let slots: Vec<usize> = (0..objects.len()).filter(|&it| objects[it] > at).collect();
            let target = if kind == 3 && !slots.is_empty() {
                Target::Object(slots[below(tc, slots.len() as u32) as usize])
            } else if !later.is_empty() {
                Target::Direct(later[below(tc, later.len() as u32) as usize])
            } else {
                return Op::Print(small);
            };
            Op::Call(Call {
                target,
                arg: if below(tc, 2) == 0 { Arg::Own } else { Arg::Const(small) },
                cleanup: maybe(tc, 10 + small),
                guard: maybe(tc, below(tc, 3) as u8),
                used: below(tc, 2) == 0,
            })
        }
    }
}

fn drawn(tc: &TestCase) -> Program {
    let funcs = 1 + below(tc, 5) as usize;
    let objects: Vec<usize> = (0..below(tc, 3)).map(|_| below(tc, funcs as u32) as usize).collect();
    let funcs = (0..funcs)
        .map(|at| Func {
            ops: (0..below(tc, 5)).map(|_| drawn_op(tc, at, funcs, &objects)).collect(),
            adds: below(tc, 3) as u8,
        })
        .collect();
    Program { funcs, objects }
}

/// The refinement: a program optimized on the facts proved of its functions is seen to do
/// what the program as written does: the same output, the same panic, the same hang. For
/// random small programs (calls by name and through objects, cleanups, guards, panics and
/// hangs that depend on the argument) and every argument, with every [`Spend`] made wherever
/// [`Facts::license`] allows it.
#[hegel::test(test_cases = 2000)]
fn a_program_optimized_on_facts_behaves_as_written(tc: TestCase) {
    let program = drawn(&tc);
    if let Err(what) = refines(&program, NONE) {
        panic!("{what}");
    }
}

/// The invariant: a body's key covers every fact its passes relied on. For random pairs of
/// programs that differ in one function, a function with the same code as written and the
/// same recorded digests in both has the same optimized code in both.
#[hegel::test(test_cases = 2000)]
fn one_key_is_one_optimized_code(tc: TestCase) {
    let a = drawn(&tc);
    let mut b = a.clone();
    let changed = below(&tc, a.funcs.len() as u32) as usize;
    let op = drawn_op(&tc, changed, a.funcs.len(), &a.objects);
    b.funcs[changed].ops.insert(0, op);
    if let Err(what) = covered(&a, &b, NONE) {
        panic!("{what}");
    }
}

fn call(target: Target) -> Call {
    Call { target, arg: Arg::Own, cleanup: None, guard: None, used: false }
}

fn func(ops: &[Op]) -> Func {
    Func { ops: ops.to_vec(), adds: 0 }
}

/// Control: with a call through an object not counted against "cannot panic", `f1`, which
/// only calls an object's method, is proved unable to panic; `f0` loses the cleanup of its
/// call of `f1`; the object's method panics, and the oracle sees the cleanup not run.
#[test]
fn the_oracle_catches_an_unwind_edge_removed_for_a_callee_that_panics_through_a_dyn_call() {
    let program = Program {
        funcs: vec![
            func(&[Op::Call(Call { cleanup: Some(7), ..call(Target::Direct(1)) })]),
            func(&[Op::Call(call(Target::Object(0)))]),
            func(&[Op::PanicIf(0)]),
        ],
        objects: vec![2],
    };
    assert_eq!(program.run(0), Seen { out: vec![7], end: End::Panicked });
    refines(&program, NONE).expect("sound as it is");
    let (kept, _) = optimized(&program, NONE);
    assert_eq!(kept, program, "the edge stays: nothing is proved of a function that calls an object");
    let planted = Plants { prove: ProvePlant::IndirectIgnored, ..NONE };
    let (after, _) = optimized(&program, planted);
    assert_eq!(after.funcs[0].ops, [Op::Call(call(Target::Direct(1)))], "the plant removes it");
    assert!(refines(&program, planted).is_err(), "the oracle catches the plant");
}

/// Control: with purity alone asked of a hoisted call, a call of a pure function that panics
/// on 0, guarded so that it runs only on 1, is made above its guard; on 0 the program as
/// written returns and the optimized one panics.
#[test]
fn the_oracle_catches_a_pure_call_hoisted_above_the_branch_guarding_its_panic() {
    let program = Program {
        funcs: vec![
            func(&[Op::Call(Call { guard: Some(1), ..call(Target::Direct(1)) }), Op::Print(2)]),
            func(&[Op::PanicIf(0)]),
        ],
        objects: Vec::new(),
    };
    assert_eq!(program.run(0), Seen { out: vec![2], end: End::Returned(0) });
    refines(&program, NONE).expect("sound as it is");
    let (kept, _) = optimized(&program, NONE);
    assert_eq!(kept, program, "the call stays under its guard: it may panic");
    let planted = Plants { spend: SpendPlant::HoistAsksPureOnly, ..NONE };
    let (after, _) = optimized(&program, planted);
    assert_eq!(after.run(0).end, End::Panicked, "the plant hoists it");
    assert!(refines(&program, planted).is_err(), "the oracle catches the plant");
}

/// Control: with a relied fact not recorded, `f0` has one key in two programs, one where
/// `f1` cannot panic (the cleanup of its call goes) and one where it can (it stays): one
/// key, two codes, which the oracle refuses.
#[test]
fn the_oracle_catches_a_fact_relied_on_but_missing_from_the_key() {
    let caller = func(&[Op::Call(Call { cleanup: Some(7), used: true, ..call(Target::Direct(1)) })]);
    let quiet = Program { funcs: vec![caller.clone(), func(&[])], objects: Vec::new() };
    let panics = Program { funcs: vec![caller, func(&[Op::PanicIf(0)])], objects: Vec::new() };
    covered(&quiet, &panics, NONE).expect("covered as it is");
    let (_, recorded) = optimized(&quiet, NONE);
    assert_eq!(
        recorded[0].relied,
        [quiet.facts(ProvePlant::Off)[1].relied(Fact::NoUnwind)],
        "the caller's key names the one fact it relied on"
    );
    let planted = Plants { spend: SpendPlant::RelyNotRecorded, ..NONE };
    assert!(covered(&quiet, &panics, planted).is_err(), "the oracle catches the plant");
}

/// What each change does, on the smallest program that has it.
#[test]
fn each_spend_changes_the_code_it_is_licensed_for() {
    let pure = func(&[]);
    // A repeated call of a pure function prints the first's result.
    let twice = Call { used: true, ..call(Target::Direct(1)) };
    let program = Program {
        funcs: vec![func(&[Op::Call(twice), Op::Call(twice)]), pure.clone()],
        objects: Vec::new(),
    };
    let (after, _) = optimized(&program, NONE);
    assert_eq!(after.funcs[0].ops, [Op::Call(twice), Op::PrintLast]);
    assert_eq!(after.run(3), program.run(3));
    // A call nobody reads of a function that is pure, cannot panic and returns goes.
    let program =
        Program { funcs: vec![func(&[Op::Call(call(Target::Direct(1)))]), pure], objects: Vec::new() };
    let (after, done) = optimized(&program, NONE);
    assert!(after.funcs[0].ops.is_empty());
    assert_eq!(done[0].relied.len(), 3, "pure, cannot panic, returns: each recorded");
    // One that prints stays: it is not pure.
    let program = Program {
        funcs: vec![func(&[Op::Call(call(Target::Direct(1)))]), func(&[Op::Print(1)])],
        objects: Vec::new(),
    };
    assert_eq!(optimized(&program, NONE).0, program);
}
