//! The `Specialize` pass over fixtures, through the optimized-body queries a build asks: which
//! calls get a copy of their callee and what the copy knows, that a copy computes what its
//! callee does, that a copy's own calls of itself call it, and the budgets. An integration test
//! (`tests/mir/main.rs` names this file), so the test database's queries are of the one copy
//! of hir-ty-mir the tests call.

use hir_expand::fixture;
use hir_ty::{db::HirDatabase, next_solver::Ty};
use hir_ty_mir::mir::{
    ArgFact, CloneId, MAX_COPIES, MAX_COPIES_PER_BODY, MirBody, MirOptLevel, MirOptions, MirSpan,
    Operand, Place, Rvalue, Specialization, Statement, StatementKind, interpret_mir,
    specialized_mir_body,
};

use crate::{db::TestDB, function};

/// What the code generator asks at `opt-level=3`, in a build that is not incremental.
fn release() -> MirOptions {
    let level = MirOptLevel::Release;
    MirOptions { ub_checks: false, lifetime_markers: false, inline: true, ..MirOptions::at(level) }
}

/// Folding only: a copy as the interpreter runs a runtime body.
fn off() -> MirOptions {
    let level = MirOptLevel::Off;
    MirOptions { ub_checks: false, lifetime_markers: false, inline: false, ..MirOptions::at(level) }
}

#[track_caller]
fn with_db<T>(fixture: &str, check: impl FnOnce(&TestDB) -> T) -> T {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || check(&db))
}

#[track_caller]
fn optimized<'db>(db: &'db TestDB, name: &str) -> &'db MirBody<'db> {
    db.optimized_mir_body(function(db, name).into(), release())
        .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"))
}

#[track_caller]
fn copy_body<'db>(
    db: &'db TestDB,
    spec: &Specialization,
    options: MirOptions,
) -> &'db MirBody<'db> {
    specialized_mir_body(db, CloneId::new(db, spec.clone()), options)
        .unwrap_or_else(|error| panic!("the copy {spec:?}: {error:?}"))
}

/// What each call of `name` that gets a copy knows, by block.
#[track_caller]
fn specialized(db: &TestDB, name: &str) -> Vec<Specialization> {
    optimized(db, name).inlined.specialized.iter().map(|it| it.clone.clone()).collect()
}

fn discriminant_reads(body: &MirBody<'_>) -> usize {
    let statements = body.basic_blocks.iter().flat_map(|(_, it)| &it.statements);
    statements
        .filter(|it| matches!(it.kind, StatementKind::Assign(_, Rvalue::Discriminant(_))))
        .count()
}

/// The reads of the discriminant of `body`'s parameter `index`.
fn discriminant_reads_of(body: &MirBody<'_>, index: usize) -> usize {
    let param = body.param_locals[index];
    let statements = body.basic_blocks.iter().flat_map(|(_, it)| &it.statements);
    let of_param = |it: &&Statement| match &it.kind {
        StatementKind::Assign(_, Rvalue::Discriminant(read)) => read.local == param,
        _ => false,
    };
    statements.filter(of_param).count()
}

fn statements(body: &MirBody<'_>) -> usize {
    body.basic_blocks.iter().map(|(_, it)| it.statements.len()).sum()
}

/// What `body` returns given `args`: its parameters assigned them at the start, as a body of no
/// parameters the interpreter runs.
#[track_caller]
fn result<'db>(db: &'db TestDB, body: &MirBody<'db>, args: &[Operand]) -> Box<[u8]> {
    let mut body = body.clone();
    let assigned = body.param_locals.iter().zip(args).map(|(&local, arg)| Statement {
        kind: StatementKind::Assign(Place::from(local).store(), Rvalue::Use(arg.clone())),
        span: MirSpan::Unknown,
    });
    let assigned: Vec<_> = assigned.collect();
    let start = body.start_block;
    for (at, statement) in assigned.into_iter().enumerate() {
        body.basic_blocks[start].statements.insert(at, statement);
    }
    body.param_locals.clear();
    let body: &'db MirBody<'db> = Box::leak(Box::new(body));
    let (value, _) = interpret_mir(db, body, None).expect("the body runs");
    value.expect("the body returns").memory.clone()
}

fn param_ty<'db>(body: &MirBody<'db>, index: usize) -> Ty<'db> {
    body.locals[body.param_locals[index]].ty.as_ref()
}

const MODES: &str = r#"
//- minicore: sized, option
pub enum Mode { Fast, Slow, Checked }

#[inline(never)]
fn run(mode: Mode, n: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        acc = match mode {
            Mode::Fast => acc + i,
            Mode::Slow => (acc << 1) ^ (i + 7),
            Mode::Checked => match Some(acc ^ (i * 3 + 1)) {
                Some(it) => it,
                None => 0,
            },
        };
        i = i + 1;
    }
    acc
}

#[inline(never)]
fn scaled(k: u32, n: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        if k == 0 { acc = acc ^ i; } else { acc = acc + i * k + 1; }
        i = i + 1;
    }
    acc
}

pub fn fast(n: u32) -> u32 { run(Mode::Fast, n) }
pub fn either(slow: bool, n: u32) -> u32 {
    if slow { run(Mode::Slow, n) } else { run(Mode::Fast, n) }
}
pub fn unknown(mode: Mode, n: u32) -> u32 { run(mode, n) }
pub fn through_pointer(n: u32) -> u32 {
    let f: fn(Mode, u32) -> u32 = run;
    f(Mode::Fast, n)
}
pub fn by_five(n: u32) -> u32 { scaled(5, n) }
pub fn by_six(n: u32) -> u32 { scaled(6, n) }
"#;

#[test]
fn a_call_passing_a_mode_calls_a_copy_without_the_mode_branch() {
    with_db(MODES, |db| {
        let run = function(db, "run");
        let fast = Specialization { base: run, facts: Box::new([(0, ArgFact::Is(0))]) };
        assert_eq!(specialized(db, "fast"), [fast.clone()]);
        let own = optimized(db, "run");
        let copy = copy_body(db, &fast, release());
        assert!(discriminant_reads(own) > 0, "{own:?}");
        assert_eq!(discriminant_reads(copy), 0, "the copy still reads the mode: {copy:?}");
        assert!(statements(copy) < statements(own), "{copy:?}");

        // Lowering reads every enum's discriminant into one temporary, the mode's and the
        // `Option`'s alike. A copy folded only, which no later pass cleans, reads the mode's no
        // more, and still the `Option`'s.
        let base = db.runtime_mir_body(run.into()).expect("the runtime MIR of `run`");
        let checked = Specialization { base: run, facts: Box::new([(0, ArgFact::Is(2))]) };
        let copy = copy_body(db, &checked, off());
        assert!(discriminant_reads_of(base, 0) > 0, "{base:?}");
        assert_eq!(discriminant_reads_of(copy, 0), 0, "the copy still reads the mode: {copy:?}");
        assert!(discriminant_reads(copy) > 0, "the `Option`'s read is gone: {copy:?}");

        let slow = Specialization { base: run, facts: Box::new([(0, ArgFact::Is(1))]) };
        let mut either = specialized(db, "either");
        either.sort_by_key(|it| format!("{it:?}"));
        assert_eq!(either, [fast, slow]);

        assert!(specialized(db, "unknown").is_empty(), "a mode not known at the call");
        assert!(specialized(db, "through_pointer").is_empty(), "a call through a function pointer");
    });
}

#[test]
fn values_the_callee_does_not_compare_with_share_one_copy() {
    with_db(MODES, |db| {
        let scaled = function(db, "scaled");
        let not_zero = Specialization {
            base: scaled,
            facts: Box::new([(0, ArgFact::IsNot(Box::new([0u128])))]),
        };
        assert_eq!(specialized(db, "by_five"), [not_zero.clone()]);
        assert_eq!(specialized(db, "by_six"), [not_zero.clone()]);
    });
}

#[test]
fn a_copy_computes_what_its_callee_does() {
    with_db(MODES, |db| {
        let run = function(db, "run");
        let base = db.runtime_mir_body(run.into()).expect("the runtime MIR of `run`");
        let n = Operand::from_bytes(Box::new(10u32.to_le_bytes()), param_ty(base, 1));
        for (discriminant, tag) in [(0u128, 0u8), (1, 1), (2, 2)] {
            let spec =
                Specialization { base: run, facts: Box::new([(0, ArgFact::Is(discriminant))]) };
            let args = [Operand::from_bytes(Box::new([tag]), param_ty(base, 0)), n.clone()];
            let expected = result(db, base, &args);
            for options in [off(), release()] {
                let copy = copy_body(db, &spec, options);
                assert_eq!(result(db, copy, &args), expected, "{spec:?} at {options:?}");
            }
        }

        let scaled = function(db, "scaled");
        let base = db.runtime_mir_body(scaled.into()).expect("the runtime MIR of `scaled`");
        let not_zero = Specialization {
            base: scaled,
            facts: Box::new([(0, ArgFact::IsNot(Box::new([0u128])))]),
        };
        let copy = copy_body(db, &not_zero, off());
        for k in [5u32, 6] {
            let k = Operand::from_bytes(Box::new(k.to_le_bytes()), param_ty(base, 0));
            let args = [k, n.clone()];
            assert_eq!(result(db, copy, &args), result(db, base, &args));
        }
    });
}

#[test]
fn a_copy_calls_itself_where_it_passes_on_what_it_knows() {
    let fixture = r#"
//- minicore: sized
pub enum Mode { Fast, Slow, Checked }

#[inline(never)]
fn walk(mode: Mode, depth: u32) -> u32 {
    if depth == 0 { return 0; }
    let mut step = 0;
    let mut i = 0;
    while i < depth {
        step = match mode { Mode::Fast => step + 1, Mode::Slow => step * 3 + 2, Mode::Checked => step ^ 5 };
        i = i + 1;
    }
    step + walk(mode, depth - 1)
}

pub fn walk_fast(depth: u32) -> u32 { walk(Mode::Fast, depth) }
"#;
    with_db(fixture, |db| {
        let walk = function(db, "walk");
        let fast = Specialization { base: walk, facts: Box::new([(0, ArgFact::Is(0))]) };
        assert_eq!(specialized(db, "walk_fast"), [fast.clone()]);
        let copy = copy_body(db, &fast, release());
        let calls: Vec<_> = copy.inlined.specialized.iter().map(|it| it.clone.clone()).collect();
        assert_eq!(calls, [fast], "the recursive call of the copy calls the copy");
        assert!(specialized(db, "walk").is_empty(), "`walk` knows nothing of its mode");
    });
}

#[test]
fn a_parameter_its_callee_writes_or_borrows_gets_no_copy() {
    let fixture = r#"
//- minicore: sized
#[inline(never)]
fn written(mut on: bool, n: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        if on { acc = acc + i; } else { acc = acc ^ (i * 3 + 1); }
        on = !on;
        i = i + 1;
    }
    acc
}

fn peek(it: &bool) -> bool { *it }

#[inline(never)]
fn borrowed(on: bool, n: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        if on { acc = acc + i; } else { acc = acc ^ (i * 3 + 1); }
        if peek(&on) { acc = acc + 1; }
        i = i + 1;
    }
    acc
}

pub fn calls_written(n: u32) -> u32 { written(true, n) }
pub fn calls_borrowed(n: u32) -> u32 { borrowed(true, n) }
"#;
    with_db(fixture, |db| {
        assert!(specialized(db, "calls_written").is_empty(), "`on` changes in the loop");
        assert!(specialized(db, "calls_borrowed").is_empty(), "`on` is read through a borrow");
    });
}

#[test]
fn copies_stay_within_their_budgets() {
    // One body calls more callees that would each get a copy than it may call copies: `f{k}`
    // removes `k + 1` operations, and `light`, called first, fewer than any. The body takes the
    // copies that remove the most, not those its first blocks call.
    let callees = MAX_COPIES_PER_BODY + 1;
    let mut fixture = String::from("//- minicore: sized\n");
    let callee = |name: &str, work: &str| {
        format!(
            "#[inline(never)]\nfn {name}(on: bool, n: u32) -> u32 {{
                let mut acc = 0; let mut i = 0;
                while i < n {{ if on {{ acc = acc + i; }} else {{ {work} }} i = i + 1; }}
                acc
            }}\n"
        )
    };
    for k in 0..callees {
        let work: String = (0..=k).map(|j| format!("acc = acc ^ (i ^ {j}); ")).collect();
        fixture += &callee(&format!("f{k}"), &work);
        // The same callee each time: copies that weigh the same.
        fixture += &callee(&format!("g{k}"), "acc = acc ^ (i ^ 7);");
    }
    fixture += &callee("light", "acc = acc ^ 1;");
    let calls =
        |name: &str| (0..callees).map(|k| format!("{name}{k}(true, n)")).collect::<Vec<_>>();
    fixture +=
        &format!("pub fn many(n: u32) -> u32 {{ light(true, n) + {} }}\n", calls("f").join(" + "));
    fixture += &format!("pub fn ties(n: u32) -> u32 {{ {} }}\n", calls("g").join(" + "));
    // A callee with more parameters it branches on than its copies may know.
    fixture += r#"
#[inline(never)]
fn three(a: bool, b: bool, c: bool, n: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        if a { acc = acc + 1; } else { acc = acc ^ 3; }
        if b { acc = acc + 2; } else { acc = acc ^ 5; }
        if c { acc = acc + 3; } else { acc = acc ^ 7; }
        i = i + 1;
    }
    acc
}
pub fn all_three(n: u32) -> u32 { three(true, false, true, n) }

#[inline(never)]
fn pick(b: bool) -> u32 { if b { 1 } else { 2 } }
pub fn picks() -> u32 { pick(true) }
"#;
    with_db(&fixture, |db| {
        let many = specialized(db, "many");
        assert_eq!(many.len(), MAX_COPIES_PER_BODY, "{many:?}");
        let (light, f0) = (function(db, "light"), function(db, "f0"));
        let left = |it: &Specialization| it.base == light || it.base == f0;
        assert!(!many.iter().any(left), "the two that remove least are left: {many:?}");
        // Which of them would be taken first is block order, which UB checks change.
        assert!(specialized(db, "ties").is_empty(), "copies tied past the budget are all left");

        // Each `bool` known is one of 3 states (true, false, not known): two give 3 * 3 - 1
        // copies, three would give 3 * 3 * 3 - 1.
        assert!(3u32.pow(2) - 1 <= MAX_COPIES && 3u32.pow(3) - 1 > MAX_COPIES);
        let three = Specialization {
            base: function(db, "three"),
            facts: Box::new([(0, ArgFact::Is(1)), (1, ArgFact::Is(0))]),
        };
        assert_eq!(specialized(db, "all_three"), [three]);

        assert!(specialized(db, "picks").is_empty(), "one branch, no loop: too little work");
    });
}
