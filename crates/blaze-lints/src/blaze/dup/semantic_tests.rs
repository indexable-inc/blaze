//! `same_behaviour`: the canonical form over plain graphs (a property of idempotence and of
//! insensitivity to the order of blocks and the numbers of temporaries, and the commutative
//! rule), and the lint over crates: where it fires, where it does not, and that two runs
//! agree.

use std::alloc::Global;

use hegel::generators as gs;
use hir_expand::fixture;

use super::{Block, Body, Operand, Place, Rvalue, Stmt, Term, body_semantic, canonical, fingerprint};
use crate::{
    query,
    tests::{CLIPPY_OFF, lint, main_crate},
};

const HEADER: &str = "//- minicore: sized, copy, clone, option, fn, eq, ord, add, mul, builtin_impls, matches, panic\n//- /src/lib.rs crate:main\n";

/// What `blaze::same_behaviour` alone says of `main`.
fn found(main: &str) -> String {
    lint(&format!("{HEADER}{}", main.trim_start_matches('\n')), &format!("{CLIPPY_OFF} --warn=blaze::same_behaviour"))
}

fn fires(main: &str) -> bool {
    found(main).contains("same compiled behaviour, written differently")
}

fn vec_of<T>(items: impl IntoIterator<Item = T>) -> Vec<T, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(items);
    out
}

fn read(local: u32) -> Operand {
    Operand::Read(Place::whole(local))
}

/// `_0 = op(_1, _2)` for an operation `head` that commutes or not, then return.
fn binary(head: u64, commutes: bool, a: u32, b: u32) -> Body {
    Body {
        params: 2,
        locals: vec_of([1, 1, 1]),
        entry: 0,
        blocks: vec_of([Block {
            stmts: vec_of([Stmt::Assign(
                Place::whole(0),
                Rvalue::Op { head, args: vec_of([read(a), read(b)]), commutes },
            )]),
            term: Term::Return,
        }]),
    }
}

#[test]
fn commutative_operands_are_one_order_and_others_are_kept() {
    let canon = |body: &Body| canonical(body).map(|it| fingerprint(&it));
    assert_eq!(canon(&binary(7, true, 1, 2)), canon(&binary(7, true, 2, 1)), "`a + b` is `b + a`");
    assert_ne!(canon(&binary(8, false, 1, 2)), canon(&binary(8, false, 2, 1)), "`a - b` is not `b - a`");
}

#[test]
fn an_operation_through_a_temporary_is_the_operation() {
    // bb0: _3 = op(_1, _2); _0 = _3; return
    let through = Body {
        params: 2,
        locals: vec_of([1, 1, 1, 1]),
        entry: 0,
        blocks: vec_of([Block {
            stmts: vec_of([
                Stmt::Assign(Place::whole(3), Rvalue::Op { head: 8, args: vec_of([read(1), read(2)]), commutes: false }),
                Stmt::Assign(Place::whole(0), Rvalue::Use(read(3))),
            ]),
            term: Term::Return,
        }]),
    };
    assert_eq!(canonical(&through).map(|it| fingerprint(&it)), canonical(&binary(8, false, 1, 2)).map(|it| fingerprint(&it)));
    // A write of an operand between the two keeps them apart.
    let mut written = through.clone();
    written.blocks[0].stmts.insert(1, Stmt::Assign(Place::whole(1), Rvalue::Use(Operand::Const(4))));
    let mut late = binary(8, false, 1, 2);
    late.blocks[0].stmts.insert(0, Stmt::Assign(Place::whole(1), Rvalue::Use(Operand::Const(4))));
    assert_ne!(canonical(&written), canonical(&late));
}

#[test]
fn a_jump_through_empty_blocks_and_a_negated_switch_are_the_plain_switch() {
    // bb0: _3 = !_1; switch _3 [0 => bb1] otherwise bb2; bb1: goto bb3; bb2: _0 = _2; return;
    // bb3: _0 = 5; return
    let negated = Body {
        params: 2,
        locals: vec_of([1, 2, 1, 2]),
        entry: 0,
        blocks: vec_of([
            Block {
                stmts: vec_of([Stmt::Assign(Place::whole(3), Rvalue::Not(read(1)))]),
                term: Term::Switch { discr: read(3), on_bool: true, arms: vec_of([(0, 1)]), otherwise: 2 },
            },
            Block { stmts: Vec::new_in(Global), term: Term::Goto(3) },
            Block { stmts: vec_of([Stmt::Assign(Place::whole(0), Rvalue::Use(read(2)))]), term: Term::Return },
            Block { stmts: vec_of([Stmt::Assign(Place::whole(0), Rvalue::Use(Operand::Const(5)))]), term: Term::Return },
        ]),
    };
    // bb0: switch _1 [0 => bb1] otherwise bb2; bb1: _0 = _2; return; bb2: _0 = 5; return
    let plain = Body {
        params: 2,
        locals: vec_of([1, 2, 1]),
        entry: 0,
        blocks: vec_of([
            Block {
                stmts: Vec::new_in(Global),
                term: Term::Switch { discr: read(1), on_bool: true, arms: vec_of([(0, 1)]), otherwise: 2 },
            },
            Block { stmts: vec_of([Stmt::Assign(Place::whole(0), Rvalue::Use(read(2)))]), term: Term::Return },
            Block { stmts: vec_of([Stmt::Assign(Place::whole(0), Rvalue::Use(Operand::Const(5)))]), term: Term::Return },
        ]),
    };
    assert_eq!(canonical(&negated), canonical(&plain));
}

fn below(tc: &hegel::TestCase, n: u32) -> u32 {
    tc.draw(gs::integers::<u32>().min_value(0).max_value(n.saturating_sub(1)))
}

fn draw_operand(tc: &hegel::TestCase, locals: u32) -> Operand {
    match below(tc, 3) {
        0 => read(below(tc, locals)),
        1 => Operand::Const(u64::from(below(tc, 3))),
        _ => Operand::Bool(below(tc, 2) == 1),
    }
}

/// A body of up to six blocks over up to two parameters and four temporaries, every target
/// and local in range, switch arms by distinct values.
fn draw_body(tc: &hegel::TestCase) -> Body {
    let params = below(tc, 3);
    let locals = params + 1 + below(tc, 5);
    let blocks = 1 + below(tc, 6);
    let mut out = Vec::new_in(Global);
    for _ in 0..blocks {
        let mut stmts = Vec::new_in(Global);
        for _ in 0..below(tc, 4) {
            let dest = Place::whole(below(tc, locals));
            let rvalue = match below(tc, 4) {
                0 => Rvalue::Use(draw_operand(tc, locals)),
                1 => Rvalue::Op {
                    head: u64::from(below(tc, 3)),
                    args: vec_of([draw_operand(tc, locals), draw_operand(tc, locals)]),
                    commutes: below(tc, 2) == 1,
                },
                2 => Rvalue::Not(draw_operand(tc, locals)),
                _ => Rvalue::BoolEq {
                    negated: below(tc, 2) == 1,
                    a: draw_operand(tc, locals),
                    b: draw_operand(tc, locals),
                },
            };
            stmts.push(Stmt::Assign(dest, rvalue));
        }
        let term = match below(tc, 5) {
            0 => Term::Goto(below(tc, blocks)),
            1 => {
                let on_bool = below(tc, 2) == 1;
                let mut arms = Vec::new_in(Global);
                for value in 0..below(tc, 3) {
                    arms.push((u128::from(value), below(tc, blocks)));
                }
                Term::Switch { discr: draw_operand(tc, locals), on_bool, arms, otherwise: below(tc, blocks) }
            }
            2 => Term::Return,
            3 => Term::Unreachable,
            _ => Term::Call {
                func: Operand::Const(9),
                args: vec_of([draw_operand(tc, locals)]),
                dest: Place::whole(below(tc, locals)),
                target: Some(below(tc, blocks)),
                cleanup: None,
            },
        };
        out.push(Block { stmts, term });
    }
    let mut types = Vec::new_in(Global);
    for _ in 0..locals {
        types.push(u64::from(below(tc, 3)));
    }
    Body { params, locals: types, entry: below(tc, blocks), blocks: out }
}

/// A permutation of `0..n` drawn as sort keys: `out[old]` is the new place.
fn draw_permutation(tc: &hegel::TestCase, n: u32) -> Vec<u32, Global> {
    let mut keyed = Vec::new_in(Global);
    for at in 0..n {
        keyed.push((below(tc, 1000), at));
    }
    keyed.sort_unstable();
    let mut out = vec_of((0..n).map(|_| 0));
    for (new, &(_, old)) in keyed.iter().enumerate() {
        out[old as usize] = u32::try_from(new).expect("a small body");
    }
    out
}

/// `body` with its blocks moved by `blocks` and its temporaries renumbered by `temps` (over
/// the locals past the parameters).
fn permuted(body: &Body, blocks: &[u32], temps: &[u32]) -> Body {
    let first = body.params + 1;
    let local = |it: &mut u32| {
        if *it >= first {
            *it = first + temps[(*it - first) as usize];
        }
    };
    let mut out = body.blocks.clone();
    for (old, block) in body.blocks.iter().enumerate() {
        let mut block = block.clone();
        block.term.targets_mut(&mut |it: &mut u32| *it = blocks[*it as usize]);
        for stmt in &mut block.stmts {
            stmt.locals_mut(&mut local.clone());
        }
        block.term.locals_mut(&mut local.clone());
        out[blocks[old] as usize] = block;
    }
    let mut locals = body.locals.clone();
    for (old, ty) in body.locals.iter().enumerate().skip(first as usize) {
        locals[(first + temps[old - first as usize]) as usize] = *ty;
    }
    Body { params: body.params, locals, entry: blocks[body.entry as usize], blocks: out }
}

/// The invariant: the canonical form of a body is its own canonical form, and is the same
/// for the body with its blocks in another order and its temporaries numbered otherwise.
#[hegel::test(test_cases = 256)]
fn the_canonical_form_is_idempotent_and_blind_to_block_order_and_temporary_names(tc: hegel::TestCase) {
    let body = draw_body(&tc);
    let blocks = draw_permutation(&tc, u32::try_from(body.blocks.len()).expect("a small body"));
    let temps = draw_permutation(&tc, u32::try_from(body.locals.len()).expect("a small body") - body.params - 1);
    let canon = canonical(&body);
    assert_eq!(canonical(&permuted(&body, &blocks, &temps)), canon, "{body:?}");
    if let Some(canon) = &canon {
        assert_eq!(canonical(canon).as_ref(), Some(canon), "{body:?}");
    }
}

// expectation unverified: main agent decides after first run (the match on a `bool` lowers to
// a comparison with each constant; the fold, the flip and the switch rules are what make it
// the `if`).
#[test]
fn an_if_and_a_match_on_a_bool_are_one_behaviour() {
    assert!(fires(
        r#"
pub fn pick_if(flag: bool, a: u32, b: u32) -> u32 { if flag { a * 3 + 1 } else { b * 7 + 2 } }
pub fn pick_match(flag: bool, a: u32, b: u32) -> u32 { match flag { true => a * 3 + 1, false => b * 7 + 2 } }
"#
    ));
}

// expectation unverified: main agent decides after first run (whether `return` and the tail
// value meet in one return block).
#[test]
fn an_early_return_and_an_else_are_one_behaviour() {
    assert!(fires(
        r#"
pub fn early(x: u32, limit: u32) -> u32 { if x > limit { return limit * 2 + 1; } x * 3 + 4 }
pub fn branch(x: u32, limit: u32) -> u32 { if x > limit { limit * 2 + 1 } else { x * 3 + 4 } }
"#
    ));
}

#[test]
fn swapped_operands_of_a_sum_are_one_behaviour() {
    let out = found(
        r#"
pub fn scaled(a: u64, b: u64, c: u64) -> u64 { let t = (a + b) * c; t - c / 2 }
pub fn scaled_again(a: u64, b: u64, c: u64) -> u64 { let t = (b + a) * c; t - c / 2 }
"#,
    );
    assert!(out.contains("`scaled_again` does what `scaled` does"), "{out}");
}

/// Six operations and a return: the MIR has no overflow asserts, so a body of fewer than
/// [`super::SEMANTIC_FLOOR`] plain operations is not compared at all and a test of it
/// proves nothing.
const STAGED: &str = "pub fn staged(x: u32, k: u32) -> u32 { let y = x * k; let z = y + 1; let w = z * 2; (w + x) * (k + 3) }\n";
const INLINE: &str = "pub fn inline(x: u32, k: u32) -> u32 { ((x * k + 1) * 2 + x) * (k + 3) }\n";

/// The fingerprints of the bodies of `main` (the `HEADER` before it).
fn compared(main: &str) -> Vec<Option<u64>, Global> {
    fingerprints(&format!("{HEADER}{}", main.trim_start_matches('\n')))
}

/// `main` has its bodies compared, so a finding left out is the form telling them apart.
fn all_compared(main: &str) {
    let rows = compared(main);
    assert!(rows.iter().all(Option::is_some), "every body is compared: {rows:?}");
}

#[test]
fn one_more_let_is_one_behaviour() {
    let main = format!("{STAGED}{INLINE}");
    all_compared(&main);
    assert!(fires(&main));
}

/// `is_none()` is a call of `Option::is_none`, `matches!` a switch on the discriminant: equal
/// only once the form inlines small callees, which it does not yet (the front end has no
/// cheap per-instance MIR with them inlined).
#[test]
fn is_none_and_matches_none_are_not_told_apart_yet() {
    assert!(!fires(
        r#"
pub fn empty(o: Option<u32>, a: u32, b: u32) -> u32 { if o.is_none() { a * 3 + 1 } else { b * 7 + 2 } }
pub fn empty_too(o: Option<u32>, a: u32, b: u32) -> u32 { if matches!(o, None) { a * 3 + 1 } else { b * 7 + 2 } }
"#
    ));
}

#[test]
fn a_difference_and_its_reverse_are_two_behaviours() {
    let main = r#"
pub fn ahead(a: u64, b: u64) -> u64 { let d = a - b; let e = d * 3; let f = e + a; (f ^ b) * (a + 7) }
pub fn behind(a: u64, b: u64) -> u64 { let d = b - a; let e = d * 3; let f = e + a; (f ^ b) * (a + 7) }
"#;
    all_compared(main);
    assert!(!fires(main));
}

#[test]
fn a_float_difference_and_its_reverse_are_two_behaviours() {
    let main = r#"
pub fn ahead(a: f64, b: f64) -> f64 { let d = a - b; let e = d * 3.0; let f = e + a; let g = f * b; (g + 1.0) * (a + 7.0) }
pub fn behind(a: f64, b: f64) -> f64 { let d = b - a; let e = d * 3.0; let f = e + a; let g = f * b; (g + 1.0) * (a + 7.0) }
"#;
    all_compared(main);
    assert!(!fires(main));
}

#[test]
fn getters_are_under_the_floor() {
    let main = r#"
pub struct P { x: u32 }
impl P {
    pub fn x(&self) -> u32 { self.x }
    pub fn get_x(&self) -> u32 { let v = self.x; v }
}
"#;
    let rows = compared(main);
    assert!(rows.iter().all(Option::is_none), "no getter is compared: {rows:?}");
    assert!(!fires(main));
}

#[test]
fn a_copy_of_the_same_shape_is_same_fns_finding() {
    let main = format!("{STAGED}{}", STAGED.replace("fn staged(", "fn staged_too("));
    all_compared(&main);
    assert!(!fires(&main));
}

#[test]
fn other_parameter_types_are_another_behaviour() {
    let main = format!("{STAGED}{}", INLINE.replace("u32", "u64"));
    all_compared(&main);
    assert!(!fires(&main));
}

/// The fingerprint of each body of `main`, on a database of its own.
fn fingerprints(text: &str) -> Vec<Option<u64>, Global> {
    let db = fixture::db(text).into_db();
    blaze_query::db::attach(&db, || {
        let krate = main_crate(&db).base();
        vec_of(query::lint_plan(&db, krate).bodies.iter().map(|it| body_semantic(&db, it.def).as_ref().map(|row| row.hash)))
    })
}

#[test]
fn two_databases_give_the_same_fingerprints_and_findings() {
    let main = format!(
        "{STAGED}{INLINE}{}",
        "pub fn ahead(a: u64, b: u64) -> u64 { let d = a - b; let e = d * 3; let f = e + a; (f ^ b) * (a + 7) }\n"
    );
    let first = compared(&main);
    assert!(first.iter().all(Option::is_some), "every body is compared: {first:?}");
    assert_eq!(first, compared(&main));
    assert_eq!(found(&main), found(&main));
}
