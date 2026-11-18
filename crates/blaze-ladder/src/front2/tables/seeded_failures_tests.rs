//! A FAILED HEAD IS NAMED BY THE CELL THAT FAILED (head.rs `SeededHeads::drive`, lane H8).
//!
//! THE FAULT (tempo, 2026-11-18, 718 bodies): "the head of `..` failed when first frozen, and the
//! request was never told why". A cell whose freeze met a need that had failed BEFORE (the seed of
//! it answered `FreezeError::Again`) was kept as failed by ITSELF, so a later ask of it, or of any
//! cell that needed it, answered `Again` naming a cell no failure was ever handed for: front2
//! tables/seeds.rs `RequestSeed::seed` hands a failure to the request only when it is not `Again`
//! (the reason was handed when the first cell failed), and names the cell `failed_since` gives.
//!
//! THE ORACLE, for every order of asks (all permutations of the fixture's cells): each answer that
//! is not `Again` hands the cell `failed_since` names (as `RequestSeed::seed` does: "told"); each
//! `Again { failed, .. }` names a told cell. The fixture is a chain: `Bad` (a field of a type no
//! crate declares), `Holder` holds a `Bad`, `Outer` a `Holder`, `f` takes an `Outer`: an order that
//! asks `Holder`, then `Outer`, then `f` met the fault (`Outer` kept as its own failure).

use blaze_own::AVec;

use super::head::{FreezeError, HeadCells, SeededHeads, SeededHeadsOpts};
use super::head_tests::{CORE_AT, a, cells, world_of};
use super::recorder::{Recorder, RecorderOpts};
use super::view::{Tables, TablesOpts};
use crate::front2::heads::Want;

const CHAIN: &str = "#![no_std]
pub struct Bad {
    pub x: Missing,
}
pub struct Holder {
    pub b: Bad,
}
pub struct Outer {
    pub h: Holder,
}
pub fn f(o: Outer) -> u32 {
    0
}
";

const LANG: &[usize] = &[CORE_AT];

/// What one order of asks answered: (asks, failures told, `Again`s that named an untold cell).
fn asked_in(order: &[Want]) -> (usize, usize, AVec<'static, (Want, Want)>) {
    let (names, sized, frame) = world_of(CHAIN);
    let g = a();
    let rec = Recorder::new(RecorderOpts { alloc: g });
    let view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &rec });
    let mut source = SeededHeads::new(SeededHeadsOpts { names: view, sized, frame, scratch: g, alloc: g, consts: None, known: None, owners: None });
    let mut told: AVec<'static, Want> = AVec::new_in(g);
    let mut untold: AVec<'static, (Want, Want)> = AVec::new_in(g);
    // Each cell asked twice: the second pass meets only kept failures.
    for w in order.iter().chain(order.iter()).copied() {
        let mark = source.failed_len();
        let error = match source.cell(w) {
            Ok(_) => continue,
            Err(error) => error,
        };
        match error {
            FreezeError::Again { failed, .. } => {
                if !told.contains(&failed) {
                    untold.push((w, failed));
                }
            }
            _ => {
                if let Some(failed) = source.failed_since(mark) {
                    told.push(failed);
                }
            }
        }
    }
    (order.len() * 2, told.len(), untold)
}

/// Every permutation of `items` (Heap's algorithm, iterative), each handed to `each`.
fn permutations(items: &mut [Want], each: &mut dyn FnMut(&[Want])) {
    let n = items.len();
    let mut c: AVec<'static, usize> = AVec::new_in(a());
    c.resize(n, 0);
    each(items);
    let mut i = 0;
    while i < n {
        if c[i] < i {
            if i % 2 == 0 {
                items.swap(0, i);
            } else {
                items.swap(c[i], i);
            }
            each(items);
            c[i] += 1;
            i = 0;
        } else {
            c[i] = 0;
            i += 1;
        }
    }
}

/// Expected line: `failed heads: P orders of N cells, T failures told, every Again names a told
/// cell`. Before the fix, an order asking `Holder`, `Outer`, `f` fails: `Again` names `Outer`.
#[test]
fn every_again_names_a_cell_whose_failure_was_told() {
    let (names, _, _) = world_of(CHAIN);
    let mut app = cells(names);
    assert!(app.len() >= 4 && app.len() <= 7, "the fixture's cells: {} (a chain of four; permutations of more are too many)", app.len());
    let (mut orders, mut told_most) = (0usize, 0usize);
    let mut bad: Option<(AVec<'static, Want>, AVec<'static, (Want, Want)>)> = None;
    permutations(&mut app, &mut |order| {
        orders += 1;
        let (_, told, untold) = asked_in(order);
        told_most = told_most.max(told);
        if !untold.is_empty() && bad.is_none() {
            let mut kept = AVec::new_in(a());
            kept.extend_from_slice(order);
            bad = Some((kept, untold));
        }
    });
    assert!(told_most > 0, "PRECONDITION: no cell of the fixture failed (`Missing` resolved, or a field of no type froze): the oracle sees nothing");
    if let Some((order, untold)) = bad {
        panic!("ORACLE: asked in the order {:?}, an `Again` named a cell whose failure was never told (asked, named): {:?}", &order[..], &untold[..]);
    }
    std::println!("failed heads: {orders} orders of {} cells, at most {told_most} failures told, every Again names a told cell", app.len());
}
