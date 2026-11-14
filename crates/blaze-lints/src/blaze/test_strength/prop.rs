//! Hegel properties of [`super::flow`], the core of the analysis, over generated bodies: an
//! assertion added never takes an observation away, and a body with every check taken out
//! observes nothing of the code under test (so its subject is pseudo-tested). The oracle is
//! [`keeps`]; [`a_lost_observation_is_caught`] plants the bug it is there to catch.

use hegel::TestCase;
use hegel::generators::{self as gs, Generator as _};

use super::flow::{self, Block, Body, Call, Effect, Outcome, Place, Src, Step, Stmt, Subject, Term};

/// The locals of a generated body: 0 is the return slot, 1 gets what the subject returns, 2 is
/// the `&mut` argument it writes through.
const LOCALS: u32 = 6;

/// The most blocks before the failing one.
const BLOCKS: u32 = 6;

fn place(tc: &TestCase) -> Place {
    let local: u32 = tc.draw(gs::integers().max_value(LOCALS - 1));
    let len: usize = tc.draw(gs::integers().max_value(2));
    let mut path = Vec::new(); 
    for _ in 0..len {
        let index: u32 = tc.draw(gs::integers().max_value(1));
        path.push(if tc.draw(gs::booleans()) { Step::Field(index) } else { Step::Variant(index) });
    }
    Place { local, path }
}

fn stmt(tc: &TestCase) -> Stmt {
    let kind: u32 = tc.draw(gs::integers().max_value(4));
    let to = place(tc);
    match kind {
        0 => Stmt::Copy { to, from: place(tc) },
        1 => Stmt::Borrow { to, of: place(tc) },
        2 => Stmt::Derive { to, from: vec![place(tc), place(tc)], mark: None }, 
        3 => {
            let step = if tc.draw(gs::booleans()) { Step::Discr } else { Step::Len };
            Stmt::Read { to, from: place(tc), step }
        }
        _ => Stmt::Aggregate {
            to,
            
            fields: vec![(vec![Step::Field(0)], place(tc)), (vec![Step::Field(1)], place(tc))],
        },
    }
}

/// An effect of a call; with `checks`, one that may check what it is handed.
fn effect(tc: &TestCase, checks: bool) -> Effect {
    let kind: u32 = tc.draw(gs::integers().max_value(if checks { 6 } else { 4 }));
    match kind {
        0 => Effect::Identity,
        1 => Effect::Element { wrap: Some(1) },
        2 => Effect::Shadow { step: Step::Len, site: 0 },
        3 => Effect::Opaque { observes: false, writes: Vec::new() }, 
        4 => Effect::Ignore,
        5 => Effect::Unwrap { variant: 1, checks: true },
        _ => Effect::Opaque { observes: true, writes: Vec::new() }, 
    }
}

/// A body whose block 0 calls the code under test; with `checks`, its branches may go to a
/// failing block (the last), and it may assert.
fn draw_body(tc: &TestCase, checks: bool) -> Body {
    let n: u32 = tc.draw(gs::integers().min_value(2).max_value(BLOCKS));
    let fail = n;
    
    let mut blocks = vec![Block {
        stmts: Vec::new(), 
        term: Term::Call(Call {
            effect: Effect::Opaque { observes: false, writes: Vec::new() }, 
            args: vec![Some(Place::local(2))], 
            dest: Place::local(1),
            target: Some(1),
            subject: Some(Subject { site: 0, mut_args: vec![0] }), 
        }),
    }];
    for at in 1..n {
        let count: usize = tc.draw(gs::integers().max_value(4));
        let stmts = (0..count).map(|_| stmt(tc)).collect(); 
        let next = at + 1;
        let kind: u32 = tc.draw(gs::integers().max_value(if checks { 4 } else { 2 }));
        let term = if next == n {
            Term::Return
        } else {
            match kind {
                0 => Term::Goto(vec![next]), 
                1 => Term::Switch { on: Some(place(tc)), targets: vec![next, n - 1] }, 
                2 => Term::Call(Call {
                    effect: effect(tc, checks),
                    args: vec![Some(place(tc)), Some(place(tc))], 
                    dest: place(tc),
                    target: Some(next),
                    subject: None,
                }),
                3 => Term::Switch { on: Some(place(tc)), targets: vec![next, fail] }, 
                _ => Term::Assert { cond: Some(place(tc)), target: next },
            }
        };
        blocks.push(Block { stmts, term });
    }
    blocks.push(Block { stmts: Vec::new(), term: Term::Fail }); 
    Body {
        locals: LOCALS,
        blocks,
        params: vec![2], 
        seed_params: false,
        ret: 0,
        ret_escapes: false,
        summaries: Vec::new(), 
    }
}

/// `body` with every check taken out: no branch fails, no assertion, no checking call.
fn unchecked(body: &Body) -> Body {
    let mut out = body.clone(); // clone: the body, changed
    let fail = (out.blocks.len() - 1) as u32;
    for block in &mut out.blocks {
        block.term = match std::mem::replace(&mut block.term, Term::Stop) {
            Term::Assert { target, .. } => Term::Goto(vec![target]), 
            Term::Switch { targets, .. } => {
                Term::Goto(targets.into_iter().filter(|&it| it != fail).collect())
            }
            Term::Fail => Term::Return,
            Term::Call(mut call) => {
                call.effect = match call.effect {
                    Effect::Unwrap { variant, .. } => Effect::Unwrap { variant, checks: false },
                    Effect::Opaque { writes, .. } => Effect::Opaque { observes: false, writes },
                    other @ (Effect::Identity
                    | Effect::Shadow { .. }
                    | Effect::Element { .. }
                    | Effect::Branch { .. }
                    | Effect::Compare { .. }
                    | Effect::Summary { .. }
                    | Effect::Ignore
                    | Effect::Fail) => other,
                };
                Term::Call(call)
            }
            other @ (Term::Goto(_) | Term::Return | Term::Stop) => other,
        };
    }
    out
}

/// The oracle: `after` keeps every observation of `before` (or a bound was hit in either, and
/// nothing is claimed).
fn keeps(before: &Outcome, after: &Outcome) -> bool {
    before.saturated || after.saturated || before.obs.is_subset(&after.obs)
}

/// `body` with an assertion on `cond` before its first return.
fn with_assert(body: &Body, cond: Place) -> Body {
    let mut out = body.clone(); // clone: the body, changed
    let added = out.blocks.len() as u32;
    if let Some(block) = out.blocks.iter_mut().find(|it| matches!(it.term, Term::Return)) {
        block.term = Term::Assert { cond: Some(cond), target: added };
    }
    out.blocks.push(Block { stmts: Vec::new(), term: Term::Return }); 
    out
}

/// The invariant: adding an assertion to a test never takes an observation away; the
/// analysis is monotone in the checks a body makes.
#[hegel::test(test_cases = 256)]
fn an_added_assertion_never_removes_an_observation(tc: TestCase) {
    let body = draw_body(&tc, true);
    let cond = place(&tc);
    let before = flow::run(&body);
    let after = flow::run(&with_assert(&body, cond));
    assert!(keeps(&before, &after), "{body:?}\nbefore {:?}\nafter {:?}", before.obs, after.obs);
}

/// The invariant: a test with every check taken out observes nothing of the code it calls;
/// whatever that code returns, the test passes, so the code is pseudo-tested.
#[hegel::test(test_cases = 256)]
fn without_checks_nothing_of_the_subject_is_observed(tc: TestCase) {
    let body = unchecked(&draw_body(&tc, true));
    let outcome = flow::run(&body);
    let observed = outcome.obs.iter().any(|it| matches!(it.src, Src::Call { .. }));
    assert!(outcome.saturated || !observed, "{body:?}\n{:?}", outcome.obs);
}

/// The planted-bug control: an analysis that loses one observation when an assertion is added
/// is caught by [`keeps`].
#[test]
fn a_lost_observation_is_caught() {
    let body = Body {
        locals: 3,
        blocks: vec![
            Block {
                stmts: Vec::new(), 
                term: Term::Call(Call {
                    effect: Effect::Opaque { observes: false, writes: Vec::new() }, 
                    args: Vec::new(), 
                    dest: Place::local(1),
                    target: Some(1),
                    subject: Some(Subject { site: 0, mut_args: Vec::new() }), 
                }),
            },
            Block {
                
                stmts: vec![Stmt::Copy { to: Place::local(2), from: Place::local(1).join(&[Step::Field(0)]) }],
                term: Term::Assert { cond: Some(Place::local(2)), target: 2 },
            },
            Block { stmts: Vec::new(), term: Term::Return }, 
        ],
        params: Vec::new(), 
        seed_params: false,
        ret: 0,
        ret_escapes: false,
        summaries: Vec::new(), 
    };
    let before = flow::run(&body);
    assert!(
        before.obs.iter().any(|it| it.src == Src::Call { site: 0, out: flow::Out::Ret }
            && it.part == [Step::Field(0), Step::Whole]),
        "an asserted field is observed whole: {:?}",
        before.obs
    );
    let mut planted = flow::run(&with_assert(&body, Place::local(2)));
    assert!(keeps(&before, &planted));
    let first = planted.obs.iter().next().cloned(); // clone: one observation
    if let Some(it) = first {
        planted.obs.remove(&it);
    }
    assert!(!keeps(&before, &planted), "the oracle misses a lost observation");
}

/// The invariant: a value nested in itself round after round (`acc = (acc, x)`) is cut at
/// [`flow::DEPTH`] and kept, not observed: unchecked, it observes nothing; asserted, all of it.
/// Hegel's first counterexample of [`without_checks_nothing_of_the_subject_is_observed`],
/// when a position past the depth was observed outright.
#[test]
fn a_value_nested_in_itself_is_kept_not_observed() {
    let body = Body {
        locals: 3,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Term::Call(Call {
                    effect: Effect::Opaque { observes: false, writes: Vec::new() },
                    args: vec![Some(Place::local(2))],
                    dest: Place::local(1),
                    target: Some(1),
                    subject: Some(Subject { site: 0, mut_args: vec![0] }),
                }),
            },
            Block {
                stmts: vec![Stmt::Aggregate {
                    to: Place::local(0),
                    fields: vec![
                        (vec![Step::Field(0)], Place::local(0)),
                        (vec![Step::Field(1)], Place::local(1)),
                    ],
                }],
                term: Term::Return,
            },
        ],
        params: vec![2],
        seed_params: false,
        ret: 0,
        ret_escapes: false,
        summaries: Vec::new(),
    };
    let unchecked = flow::run(&body);
    assert!(!unchecked.saturated, "the fixpoint ends with positions cut at the depth");
    assert!(unchecked.obs.is_empty(), "nothing is checked: {:?}", unchecked.obs);
    let checked = flow::run(&with_assert(&body, Place::local(0)));
    for out in [flow::Out::Ret, flow::Out::Arg(0)] {
        assert!(
            checked.obs.iter().any(|it| it.src == Src::Call { site: 0, out } && it.part.last() == Some(&Step::Whole)),
            "an assertion on the nest observes {out:?} whole: {:?}",
            checked.obs
        );
    }
}
