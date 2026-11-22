//! The loop-entry variant ([`crate::loop_entry`]) against the evaluator: a function run whole
//! and its variant entered at a loop header with the values the loop holds there after `k`
//! iterations return the same, for every `n` and `k` of a grid; and a planted bug of each kind
//! is caught by the same comparison.
//!
//! The values at the header are computed by a model of the loop in Rust (each test's doc says
//! it), never by a run of another compiler (LANE-RULES rule 9). Words are the convention's
//! (loop_entry.rs "The convention"): integers zero-extended, an `f64` by its bits.

use std::alloc::Global;

use crate::{
    Block, Func, Op, Ty,
    builder::{Builder, Full, Param, Variadic},
    eval::{Val, run_in},
    flags::{Flags, IntCond},
    func::{BlockKind, Conv},
    loop_entry::{LiveKey, LoopEntryError, Plant, entry_variant, entry_variant_planted},
    print,
};

const FUEL: u64 = 1_000_000;

/// A built function, the header to enter, and its loop map's keys (sorted).
struct Case {
    func: Func<Global>,
    header: Block,
    keys: std::vec::Vec<LiveKey>,
}

/// `sum(n) = 0 + 1 + .. + (n - 1)` as one loop:
///
/// ```text
/// block0: n = param; z = 0; jump block1(z, z)
/// block1: i = param; s = param; c = i <u n; br c, block2(), block3()
/// block2: s2 = s + i; one = 1; i2 = i + one; jump block1(i2, s2)
/// block3: return s
/// ```
///
/// Live at block1: `n`, `i`, `s`. After `k` iterations: `i = k`, `s = k(k-1)/2`.
fn sum_loop() -> Case {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let n = b.param(Ty::I64);
    let z = b.iconst(Ty::I64, 0);
    b.jump((header, &[z, z]));
    b.begin(header);
    let i = b.param(Ty::I64);
    let s = b.param(Ty::I64);
    let c = b.icmp(IntCond::Ult, i, n);
    b.br(Flags::NONE, c, (body, &[]), (exit, &[]));
    b.begin(body);
    let s2 = b.binary(Op::Add, Flags::NONE, s, i);
    let one = b.iconst(Ty::I64, 1);
    let i2 = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((header, &[i2, s2]));
    b.begin(exit);
    b.ret(&[s]);
    let func = b.finish().expect("sum builds");
    let mut keys = vec![LiveKey(n.0), LiveKey(i.0), LiveKey(s.0)];
    keys.sort();
    Case { func, header, keys }
}

/// Two loops; the inner header is entered, and the outer loop goes on after it:
///
/// ```text
/// block0: n = param; z = 0; jump block1(z, z)
/// block1 (outer): i = param; acc = param; c = i <u n; br c, block2(), block6()
/// block2: one = 1; bound = i + one; jump block3(z, acc)
/// block3 (inner): j = param; a = param; c2 = j <u bound; br c2, block4(), block5()
/// block4: a2 = a + j; one2 = 1; j2 = j + one2; jump block3(j2, a2)
/// block5: one3 = 1; i2 = i + one3; jump block1(i2, a)
/// block6: return acc
/// ```
///
/// `f(n) = sum over i < n of (0 + .. + i)`. Live at block3: `n`, `z`, `i`, `bound`, `j`, `a`.
/// `bound` is made in block2, which block3 reaches again by the outer back edge: the variant
/// DEMOTES it (loop_entry.rs), and a variant that does not store its new value
/// ([`Plant::NoSpill`]) keeps the entry's bound for every later outer iteration.
fn nested_loops() -> Case {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let [entry, outer, pre, inner, body, latch, exit] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let n = b.param(Ty::I64);
    let z = b.iconst(Ty::I64, 0);
    b.jump((outer, &[z, z]));
    b.begin(outer);
    let i = b.param(Ty::I64);
    let acc = b.param(Ty::I64);
    let c = b.icmp(IntCond::Ult, i, n);
    b.br(Flags::NONE, c, (pre, &[]), (exit, &[]));
    b.begin(pre);
    let one = b.iconst(Ty::I64, 1);
    let bound = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((inner, &[z, acc]));
    b.begin(inner);
    let j = b.param(Ty::I64);
    let a = b.param(Ty::I64);
    let c2 = b.icmp(IntCond::Ult, j, bound);
    b.br(Flags::NONE, c2, (body, &[]), (latch, &[]));
    b.begin(body);
    let a2 = b.binary(Op::Add, Flags::NONE, a, j);
    let one2 = b.iconst(Ty::I64, 1);
    let j2 = b.binary(Op::Add, Flags::NONE, j, one2);
    b.jump((inner, &[j2, a2]));
    b.begin(latch);
    let one3 = b.iconst(Ty::I64, 1);
    let i2 = b.binary(Op::Add, Flags::NONE, i, one3);
    b.jump((outer, &[i2, a]));
    b.begin(exit);
    b.ret(&[acc]);
    let func = b.finish().expect("nested builds");
    let mut keys = vec![LiveKey(n.0), LiveKey(z.0), LiveKey(i.0), LiveKey(bound.0), LiveKey(j.0), LiveKey(a.0)];
    keys.sort();
    Case { func, header: inner, keys }
}

/// `g(n: i32) -> f64`: `x = 0.0; for i in 0..n { x += 1.5 }`, the counter an `i32` and the
/// sum an `f64`, so the variant makes a value from its word by `trunc` and by `bitcast`:
///
/// ```text
/// block0: n = param i32; z = 0i32; x0 = 0.0; jump block1(z, x0)
/// block1: i = param i32; x = param f64; c = i <u n; br c, block2(), block3()
/// block2: h = 1.5; x2 = x + h; one = 1i32; i2 = i + one; jump block1(i2, x2)
/// block3: return x
/// ```
///
/// Live at block1: `n`, `i`, `x`. After `k` iterations: `i = k`, `x = 1.5 k` (exact in `f64`
/// for the grid's `k`).
fn float_loop() -> Case {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::F64)]);
    b.set_signature(sig);
    let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let n = b.param(Ty::I32);
    let z = b.iconst(Ty::I32, 0);
    let x0 = b.fconst(Ty::F64, u128::from(0f64.to_bits()));
    b.jump((header, &[z, x0]));
    b.begin(header);
    let i = b.param(Ty::I32);
    let x = b.param(Ty::F64);
    let c = b.icmp(IntCond::Ult, i, n);
    b.br(Flags::NONE, c, (body, &[]), (exit, &[]));
    b.begin(body);
    let h = b.fconst(Ty::F64, u128::from(1.5f64.to_bits()));
    let x2 = b.binary(Op::Fadd, Flags::NONE, x, h);
    let one = b.iconst(Ty::I32, 1);
    let i2 = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((header, &[i2, x2]));
    b.begin(exit);
    b.ret(&[x]);
    let func = b.finish().expect("float builds");
    let mut keys = vec![LiveKey(n.0), LiveKey(i.0), LiveKey(x.0)];
    keys.sort();
    Case { func, header, keys }
}

/// Runs `case`'s function on `n` and its variant (made with `plant`) on the words `words`
/// (by key); whether the two outcomes are equal, with both printed when they are not.
fn agrees(case: &Case, plant: Plant, args: &[Val], words: &[Val]) -> Result<(), std::string::String> {
    let whole = run_in(case.func.view(), args, FUEL, Global).expect("the function runs");
    let variant =
        entry_variant_planted(case.func.view(), case.header, &case.keys, plant, Global).expect("the variant is made");
    let entered = run_in(variant.view(), words, FUEL, Global).expect("the variant runs");
    match whole == entered {
        true => Ok(()),
        false => Err(format!(
            "args {args:?} words {words:?}: whole {whole:?}, entered {entered:?}\n{}",
            print(variant.view())
        )),
    }
}

/// The words of `sum_loop` after `k` iterations, by key (`n`, `i`, `s`: in value order).
fn sum_words(n: u64, k: u64) -> [Val; 3] {
    [Val::Int(n.into()), Val::Int(k.into()), Val::Int((k * k.saturating_sub(1) / 2).into())]
}

/// The words of `nested_loops` at the inner header with outer `i` and inner `j`, by key
/// (`n`, `z`, `i`, `bound`, `j`, `a`: in value order).
fn nested_words(n: u64, i: u64, j: u64) -> [Val; 6] {
    let before: u64 = (0..i).map(|it| it * (it + 1) / 2).sum();
    let a = before + j * j.saturating_sub(1) / 2;
    [Val::Int(n.into()), Val::Int(0), Val::Int(i.into()), Val::Int((i + 1).into()), Val::Int(j.into()), Val::Int(a.into())]
}

/// Every `n < 12` and every header state of the run: the variant returns the function's
/// answer. Expected: passes.
#[test]
fn the_variant_entered_at_any_iteration_returns_the_whole_functions_answer() {
    let sum = sum_loop();
    let nested = nested_loops();
    let float = float_loop();
    for n in 0u64..12 {
        for k in 0..=n {
            agrees(&sum, Plant::None, &[Val::Int(n.into())], &sum_words(n, k)).unwrap_or_else(|e| panic!("sum: {e}"));
            let x = 1.5f64 * k as f64;
            let words = [Val::Int(n.into()), Val::Int(k.into()), Val::Int(x.to_bits().into())];
            agrees(&float, Plant::None, &[Val::Int(n.into())], &words).unwrap_or_else(|e| panic!("float: {e}"));
        }
        for i in 0..n {
            for j in 0..=i + 1 {
                agrees(&nested, Plant::None, &[Val::Int(n.into())], &nested_words(n, i, j))
                    .unwrap_or_else(|e| panic!("nested: {e}"));
            }
        }
    }
    println!("loop entry: sum, nested and float variants agree with the whole function");
}

/// The controls: a variant that does not store a demoted value's new definition, and one that
/// takes its words a key late, each differ from the function on some state of the grid.
/// Expected: passes (each plant is caught).
#[test]
fn a_planted_variant_is_caught() {
    let nested = nested_loops();
    let caught = (0u64..8).any(|n| {
        (0..n).any(|i| (0..=i + 1).any(|j| agrees(&nested, Plant::NoSpill, &[Val::Int(n.into())], &nested_words(n, i, j)).is_err()))
    });
    assert!(caught, "a variant that does not store `bound` again agreed on every state");
    let sum = sum_loop();
    let caught = (1u64..8).any(|n| (0..n).any(|k| agrees(&sum, Plant::RowsRotated, &[Val::Int(n.into())], &sum_words(n, k)).is_err()));
    assert!(caught, "a variant taking its words a key late agreed on every state");
    println!("loop entry: NoSpill and RowsRotated caught");
}

/// A loop map that misses a live value is refused by name, never guessed: `sum_loop` without
/// `n` (used in the header's comparison, made in the entry). Expected: `NotInMap { value: n }`.
#[test]
fn a_value_missing_from_the_loop_map_is_refused() {
    let sum = sum_loop();
    let n = sum.keys[0];
    let without: std::vec::Vec<LiveKey> = sum.keys[1..].to_vec();
    let refused = entry_variant(sum.func.view(), sum.header, &without, Global).map(|_| ());
    assert_eq!(refused, Err(LoopEntryError::NotInMap { value: n.0 }));
    let unsorted = [sum.keys[1], sum.keys[0], sum.keys[2]];
    let refused = entry_variant(sum.func.view(), sum.header, &unsorted, Global).map(|_| ());
    assert_eq!(refused, Err(LoopEntryError::Unsorted { key: sum.keys[0].0 }));
    println!("loop entry: a missing value and an unsorted map refused");
}
