//! By-value binding of a whole local scrutinee inside an or-pattern (walk/moves.rs `move_out`,
//! walk.rs `scan_flags`, walk/moves.rs `scan_local_scrutinee`):
//! - `match item { item @ E::A(_) | item @ E::B(_) => ..., E::C(y, _) => ... }` (the
//!   `verus_state_machines_macros` `parse_result_to_smir` shape),
//! - nested or-patterns `(x @ E::A(_) | x @ E::B(_)) | x @ E::C(_)`,
//! - `if let x @ E::A(_) | x @ E::B(_) = item { ... } else { ... }`,
//! - `let (x @ E::A(_) | x @ E::B(_)) = item else { return ...; };`,
//! - field scrutinee `match s.item { x @ E::A(_) | x @ E::B(_) => ..., ... }`,
//! - loop re-initialization combining partial move and whole-local or-pattern move,
//! - twin: a guarded arm with a by-value drop binding inside an or-pattern is cleanly refused.
//!
//! Built and run natively via front2 `run_with` with drop-counter probes asserting exact drop
//! count == 1 (zero double-drops, zero leaks) on every path.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct Probe {
    id: u8,
    counts: *mut [u8; 8],
    order: *mut [u8; 16],
    order_len: *mut usize,
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            (*self.counts)[self.id as usize] += 1;
            let idx = *self.order_len;
            if idx < 16 {
                (*self.order)[idx] = self.id;
                *self.order_len = idx + 1;
            }
        }
    }
}

enum Item {
    A(Probe),
    B(Probe),
    C(Probe, Probe),
}

struct Holder {
    item: Item,
    other: Probe,
}

fn mk(id: u8, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> Probe {
    Probe { id, counts, order, order_len }
}

fn tag_of(item: &Item) -> u8 {
    match item {
        Item::A(_) => 1,
        Item::B(_) => 2,
        Item::C(_, _) => 3,
    }
}

fn consume_item(item: Item) -> u8 {
    tag_of(&item)
}

fn consume_probe(p: Probe) -> u8 {
    p.id
}
"#;

fn run(rest: &str) -> Result<u8, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    let alloc = blaze_own::testing::heap().shared();
    let mut text = blaze_own::String::new_in(alloc);
    text.push_str(HEAD);
    text.push_str(rest);
    let files = [SourceFile { path: "src/main.rs", text: text.as_str() }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), alloc).map_err(|e| {
        use core::fmt::Write as _;
        let mut out = blaze_own::String::new_in(alloc);
        let _ = write!(&mut out, "{e}");
        out
    })
}

/// `match item { item @ Item::A(_) | item @ Item::B(_) => ..., Item::C(y, _) => ... }`
/// across all three variants (including shadowing `item` and mixing whole-local drop flag with
/// partial-move `rest` bitset on `Item::C`): every `Probe` drops exactly once, in scope order.
#[test]
fn match_or_pat_whole_local_move_all_variants() {
    let got = run(r#"
fn run_match(kind: u8, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let item = if kind == 0 {
        Item::A(mk(0, counts, order, order_len))
    } else if kind == 1 {
        Item::B(mk(1, counts, order, order_len))
    } else {
        Item::C(mk(2, counts, order, order_len), mk(3, counts, order, order_len))
    };
    let after = mk(4, counts, order, order_len);
    let res = match item {
        item @ Item::A(_) | item @ Item::B(_) => consume_item(item),
        Item::C(y, _) => consume_probe(y) + 10,
    };
    let _ = after.id;
    res
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;

    // Variant A: probe 0 drops inside consume_item, then probe 4 at scope exit; item NOT double-dropped.
    let r0 = run_match(0, &mut counts, &mut order, &mut order_len);
    if r0 != 1 || counts[0] != 1 || counts[4] != 1 || order_len != 2 || order[0] != 0 || order[1] != 4 {
        return 10;
    }

    // Variant B: probe 1 drops inside consume_item, then probe 4 at scope exit.
    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r1 = run_match(1, &mut counts, &mut order, &mut order_len);
    if r1 != 2 || counts[1] != 1 || counts[4] != 1 || order_len != 2 || order[0] != 1 || order[1] != 4 {
        return 20;
    }

    // Variant C: probe 2 moves out and drops in consume_probe; then probe 4 drops; then item's
    // remaining field probe 3 drops via rest mask (probe 2 NOT double-dropped).
    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r2 = run_match(2, &mut counts, &mut order, &mut order_len);
    if r2 != 12 || counts[2] != 1 || counts[3] != 1 || counts[4] != 1 || order_len != 3 || order[0] != 2 || order[1] != 4 || order[2] != 3 {
        return 30;
    }

    0
}
"#);
    assert_eq!(got, Ok(0), "match or-pat whole local move across A/B/C: {got:?}");
}

/// Nested or-patterns `(x @ Item::A(_) | x @ Item::B(_)) | x @ Item::C(_, _)` across all three
/// variants: exact drop count == 1 for every probe on every variant.
#[test]
fn match_nested_or_pat_whole_local_move() {
    let got = run(r#"
fn run_nested(kind: u8, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let item = if kind == 0 {
        Item::A(mk(0, counts, order, order_len))
    } else if kind == 1 {
        Item::B(mk(1, counts, order, order_len))
    } else {
        Item::C(mk(2, counts, order, order_len), mk(3, counts, order, order_len))
    };
    match item {
        (x @ Item::A(_) | x @ Item::B(_)) | x @ Item::C(_, _) => consume_item(x),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;

    let r0 = run_nested(0, &mut counts, &mut order, &mut order_len);
    if r0 != 1 || counts[0] != 1 || order_len != 1 {
        return 11;
    }

    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r1 = run_nested(1, &mut counts, &mut order, &mut order_len);
    if r1 != 2 || counts[1] != 1 || order_len != 1 {
        return 22;
    }

    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r2 = run_nested(2, &mut counts, &mut order, &mut order_len);
    if r2 != 3 || counts[2] != 1 || counts[3] != 1 || order_len != 2 {
        return 33;
    }

    0
}
"#);
    assert_eq!(got, Ok(0), "nested or-pat whole local move: {got:?}");
}

/// `if let x @ Item::A(_) | x @ Item::B(_) = item { ... } else { ... }` for matching (`A`, `B`)
/// and non-matching (`C`, both letting `item` drop at outer scope end and moving `item` in `else`).
#[test]
fn if_let_or_pat_whole_local_move() {
    let got = run(r#"
fn run_if_let(kind: u8, consume_in_else: bool, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let item = if kind == 0 {
        Item::A(mk(0, counts, order, order_len))
    } else if kind == 1 {
        Item::B(mk(1, counts, order, order_len))
    } else {
        Item::C(mk(2, counts, order, order_len), mk(3, counts, order, order_len))
    };
    if let x @ Item::A(_) | x @ Item::B(_) = item {
        consume_item(x)
    } else if consume_in_else {
        consume_item(item) + 10
    } else {
        99
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;

    // Matching A
    let r0 = run_if_let(0, false, &mut counts, &mut order, &mut order_len);
    if r0 != 1 || counts[0] != 1 || order_len != 1 {
        return 10;
    }

    // Matching B
    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r1 = run_if_let(1, false, &mut counts, &mut order, &mut order_len);
    if r1 != 2 || counts[1] != 1 || order_len != 1 {
        return 20;
    }

    // Non-matching C, outer scope drops item
    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r2 = run_if_let(2, false, &mut counts, &mut order, &mut order_len);
    if r2 != 99 || counts[2] != 1 || counts[3] != 1 || order_len != 2 {
        return 30;
    }

    // Non-matching C, else branch moves item
    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r3 = run_if_let(2, true, &mut counts, &mut order, &mut order_len);
    if r3 != 13 || counts[2] != 1 || counts[3] != 1 || order_len != 2 {
        return 40;
    }

    0
}
"#);
    assert_eq!(got, Ok(0), "if let or-pat whole local move: {got:?}");
}

/// `let (x @ Item::A(_) | x @ Item::B(_)) = item else { return ...; };` on both matching (`A`, `B`)
/// and diverging `else` (`C`) paths: exact drop count == 1 on every path.
#[test]
fn let_else_or_pat_whole_local_move() {
    let got = run(r#"
fn run_let_else(kind: u8, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let item = if kind == 0 {
        Item::A(mk(0, counts, order, order_len))
    } else if kind == 1 {
        Item::B(mk(1, counts, order, order_len))
    } else {
        Item::C(mk(2, counts, order, order_len), mk(3, counts, order, order_len))
    };
    let (x @ Item::A(_) | x @ Item::B(_)) = item else {
        return 77;
    };
    consume_item(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;

    let r0 = run_let_else(0, &mut counts, &mut order, &mut order_len);
    if r0 != 1 || counts[0] != 1 || order_len != 1 {
        return 10;
    }

    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r1 = run_let_else(1, &mut counts, &mut order, &mut order_len);
    if r1 != 2 || counts[1] != 1 || order_len != 1 {
        return 20;
    }

    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let r2 = run_let_else(2, &mut counts, &mut order, &mut order_len);
    if r2 != 77 || counts[2] != 1 || counts[3] != 1 || order_len != 2 {
        return 30;
    }

    0
}
"#);
    assert_eq!(got, Ok(0), "let-else or-pat whole local move: {got:?}");
}

/// Field scrutinee (`match holder.item { x @ Item::A(_) | x @ Item::B(_) => ..., ... }`) and loop
/// re-initialization combining partial move and whole-local or-pattern move.
#[test]
fn field_scrutinee_and_loop_reinit_or_pat_move() {
    let got = run(r#"
fn run_field(kind: u8, counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let holder = Holder {
        item: if kind == 0 {
            Item::A(mk(0, counts, order, order_len))
        } else {
            Item::B(mk(1, counts, order, order_len))
        },
        other: mk(2, counts, order, order_len),
    };
    match holder.item {
        x @ Item::A(_) | x @ Item::B(_) => consume_item(x),
        Item::C(_, _) => 9,
    }
}

fn run_loop(counts: *mut [u8; 8], order: *mut [u8; 16], order_len: *mut usize) -> u8 {
    let mut item = Item::C(mk(0, counts, order, order_len), mk(1, counts, order, order_len));
    let mut acc = 0u8;
    loop {
        match item {
            x @ Item::A(_) | x @ Item::B(_) => {
                acc += consume_item(x);
                break;
            }
            Item::C(y, _) => {
                acc += consume_probe(y) + 10;
                item = Item::B(mk(2, counts, order, order_len));
            }
        }
    }
    acc
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;

    let rf0 = run_field(0, &mut counts, &mut order, &mut order_len);
    if rf0 != 1 || counts[0] != 1 || counts[2] != 1 || order_len != 2 || order[0] != 0 || order[1] != 2 {
        return 10;
    }

    counts = [0; 8];
    order = [0; 16];
    order_len = 0;
    let rl = run_loop(&mut counts, &mut order, &mut order_len);
    // Iter 1: C(0, 1) -> move 0 (drop 0), reassign item (drops remaining 1), item = B(2).
    // Iter 2: B(2) -> whole-local move via or-pattern (drop 2), break; scope exit sees flag == 0.
    if rl != 12 || counts[0] != 1 || counts[1] != 1 || counts[2] != 1 || order_len != 3 || order[0] != 0 || order[1] != 1 || order[2] != 2 {
        return 20;
    }

    0
}
"#);
    assert_eq!(got, Ok(0), "field scrutinee & loop reinit or-pat move: {got:?}");
}

/// Twin: a guarded arm whose or-pattern moves a dropping local by value is still cleanly refused
/// (never clears the scrutinee's drop flag before the guard passes).
#[test]
fn twin_guarded_or_pat_whole_local_move_is_refused() {
    let got = run(r#"
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut counts = [0u8; 8];
    let mut order = [0u8; 16];
    let mut order_len = 0usize;
    let item = Item::A(mk(0, &mut counts, &mut order, &mut order_len));
    let r = match item {
        x @ Item::A(_) | x @ Item::B(_) if tag_of(&x) == 2 => consume_item(x),
        _ => 5,
    };
    r as i32
}
"#);
    let Err(why) = got else {
        panic!("guarded or-pattern moving a dropping value must be refused, got {got:?}");
    };
    assert!(
        why.contains("guarded arm"),
        "expected clean guarded-arm refusal, got: {why}"
    );
}
