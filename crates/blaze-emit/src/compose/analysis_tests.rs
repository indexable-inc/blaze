//! The analysis pass's test (WAVE-BACKEND.md K1; backend-next.md D100 point 5), against an
//! oracle written here from the definitions alone, over random functions of up to eight blocks
//! with loops (reducible and not), block parameters of three types and address-taken slots:
//!
//! - liveness by the textbook dataflow (a block's live-in is its uses and its live-out less its
//!   definitions, its live-out the union of its targets' live-in, iterated to a fixed point), not
//!   the pass's walk back from each use;
//! - at every instruction, the values live before or after it, and at a terminator the used
//!   parameters of each target (its edge writes them), never share a register home;
//! - no `Local` value is live into any block;
//! - every value live into a loop header just after its parameters is in that header's map at the
//!   place its home says, and the map holds nothing else but address-taken slots;
//! - the back edges are exactly the edges to a block at or before their source in `order`;
//! - `irreducible` is set exactly when the graph less its dominator back edges keeps a cycle, and
//!   then no value has a register;
//! - a register home is allocatable, of its value's class, and saved when callee-saved.
//!
//! Expected: `test compose::analysis_tests::homes_never_share_a_register_where_both_are_live ...
//! ok`, and `test compose::analysis_tests::the_oracle_catches_a_missing_loop_extension ... ok`
//! printing "plant NoLoopExtension: caught at case k of 512".

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::op::{WalkError, Word, walk};
use blaze_lir::{Block, Builder, Flags, Full, IntCond, Layout, Op, Ty, Value, View};
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape, check, control};
use blaze_tier::{ARM64, Place, RegClass};

use super::analysis::{Analysis, Home, NO_USE, Plant, analyse_planted};
use super::loop_map::slot_of;

/// A drawn block parameter's type: mostly `i64`, some `f64` (the other class), some `i128` (no
/// register class: always a cell when it crosses a block).
const TYPES: [Ty; 4] = [Ty::I64, Ty::I64, Ty::F64, Ty::I128];

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

/// `d` dominates `b` exactly when `dom[b][d]`, for a graph whose every block is reached from
/// block 0 (by the iteration on sets: `dom(b) = {b} ∪ ⋂ dom(p)` over its predecessors).
fn dominators_of(succs: &[AVec<'static, usize>]) -> AVec<'static, AVec<'static, bool>> {
    let n = succs.len();
    let alloc = Alloc::global();
    let mut dom = Vec::new_in(alloc);
    for block in 0..n {
        let mut row = Vec::from_elem_in(block != 0, n, alloc);
        row[block] = true;
        dom.push(row);
    }
    loop {
        let mut changed = false;
        for block in 1..n {
            let mut new = Vec::from_elem_in(true, n, alloc);
            for pred in 0..n {
                if succs[pred].contains(&block) {
                    for d in 0..n {
                        new[d] = new[d] && dom[pred][d];
                    }
                }
            }
            new[block] = true;
            if new.as_slice() != dom[block].as_slice() {
                dom[block] = new;
                changed = true;
            }
        }
        if !changed {
            return dom;
        }
    }
}

/// A value of `ty` defined in a block dominating `block`, or earlier in `block` (`here`).
fn pick(tape: &Tape, defined: &[AVec<'static, (Value, Ty)>], dom: &[bool], here: &[(Value, Ty)], ty: Ty) -> Value {
    let alloc = Alloc::global();
    let mut candidates: AVec<'static, Value> = Vec::new_in(alloc);
    for (block, values) in defined.iter().enumerate() {
        if dom[block] {
            candidates.extend(values.iter().filter(|it| it.1 == ty).map(|it| it.0));
        }
    }
    candidates.extend(here.iter().filter(|it| it.1 == ty).map(|it| it.0));
    candidates[tape.below(candidates.len() as u64, "operand") as usize]
}

fn args_for(
    tape: &Tape,
    params: &[Ty],
    defined: &[AVec<'static, (Value, Ty)>],
    dom: &[bool],
    here: &[(Value, Ty)],
) -> AVec<'static, Value> {
    let mut args = Vec::new_in(Alloc::global());
    for &ty in params {
        args.push(pick(tape, defined, dom, here, ty));
    }
    args
}

/// A function of `blocks` blocks: each but the last goes on to the next (so every block is
/// reached, and the forward edges alone make an order the dominators follow) and maybe to one
/// more block past the entry, forward or back; the last returns.
fn draw_func(tape: &Tape) -> Result<Func<Alloc<'static>>, Broke> {
    let alloc = Alloc::global();
    let blocks = tape.range(1..9, "blocks") as usize;
    let mut succs: AVec<'static, AVec<'static, usize>> = Vec::new_in(alloc);
    let mut params: AVec<'static, AVec<'static, Ty>> = Vec::new_in(alloc);
    for block in 0..blocks {
        let mut list = Vec::new_in(alloc);
        if block + 1 < blocks {
            list.push(block + 1);
            if tape.one_in(2, "a second successor") {
                let to = tape.range(1..blocks as u64, "its target") as usize;
                if to != block + 1 {
                    list.push(to);
                }
            }
        }
        succs.push(list);
        let mut types = Vec::new_in(alloc);
        if block > 0 {
            for _ in 0..tape.below(3, "parameters") {
                types.push(TYPES[tape.below(TYPES.len() as u64, "a parameter's type") as usize]);
            }
        }
        params.push(types);
    }
    let dom = dominators_of(&succs);
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let slot = b.slot(8, 3, SlotKind::Temp);
    let mut ids = Vec::new_in(alloc);
    for _ in 0..blocks {
        ids.push(b.create_block(BlockKind::Normal));
    }
    let mut defined: AVec<'static, AVec<'static, (Value, Ty)>> = Vec::new_in(alloc);
    for block in 0..blocks {
        b.begin(ids[block]);
        let mut here: AVec<'static, (Value, Ty)> = Vec::new_in(alloc);
        if block == 0 {
            let n = b.param(Ty::I64);
            here.push((n, Ty::I64));
            let f = b.fconst(Ty::F64, u128::from(1.5f64.to_bits()));
            here.push((f, Ty::F64));
            let wide = b.iconst(Ty::I128, 7);
            here.push((wide, Ty::I128));
        } else {
            for &ty in params[block].iter() {
                let p = b.param(ty);
                here.push((p, ty));
            }
        }
        let row = dom[block].as_slice();
        for _ in 0..tape.below(5, "instructions") {
            match tape.below(4, "an instruction") {
                0 | 1 => {
                    let x = pick(tape, &defined, row, &here, Ty::I64);
                    let y = pick(tape, &defined, row, &here, Ty::I64);
                    let v = b.binary(Op::Add, Flags::NONE, x, y);
                    here.push((v, Ty::I64));
                }
                2 => {
                    let x = pick(tape, &defined, row, &here, Ty::F64);
                    let y = pick(tape, &defined, row, &here, Ty::F64);
                    let v = b.binary(Op::Fadd, Flags::NONE, x, y);
                    here.push((v, Ty::F64));
                }
                _ => {
                    if tape.one_in(2, "a slot's address") {
                        b.slot_addr(slot);
                    } else {
                        let v = b.iconst(Ty::I64, u128::from(tape.below(100, "a constant")));
                        here.push((v, Ty::I64));
                    }
                }
            }
        }
        let targets = succs[block].as_slice();
        match targets {
            [] => {
                let v = pick(tape, &defined, row, &here, Ty::I64);
                b.ret(&[v]);
            }
            &[to] => {
                let args = args_for(tape, &params[to], &defined, row, &here);
                b.jump((ids[to], args.as_slice()));
            }
            &[then, otherwise, ..] => {
                let x = pick(tape, &defined, row, &here, Ty::I64);
                let y = pick(tape, &defined, row, &here, Ty::I64);
                let cond = b.icmp(IntCond::Ugt, x, y);
                let a = args_for(tape, &params[then], &defined, row, &here);
                let c = args_for(tape, &params[otherwise], &defined, row, &here);
                b.br(Flags::NONE, cond, (ids[then], a.as_slice()), (ids[otherwise], c.as_slice()));
            }
        }
        defined.push(here);
    }
    b.finish_checked().map_err(|error| Broke::Harness(format!("the drawn function does not build: {error:?}")))
}

/// Calls `f` with each operand word of `inst`.
fn words_of(view: View<'_>, inst: usize, mut f: impl FnMut(Word)) {
    let layout = view.op(inst).info().layout;
    let words: &[u32] = match layout {
        Layout::None => &[],
        Layout::Data(_) => core::slice::from_ref(&view.data[inst]),
        Layout::Extra(_) => &view.extra[view.data[inst] as usize..],
    };
    let walked = walk::<()>(words, layout.fields(), Ty(view.tys[inst]), view.tys, &mut |word| {
        f(word);
        Ok(())
    });
    assert!(!matches!(walked, Err(WalkError::Short | WalkError::NoScrutinee)), "a built function's operands fit");
}

/// One step back over `inst`: its result leaves `live`, its operands enter.
fn step(view: View<'_>, inst: usize, live: &mut [bool]) {
    if !view.ty(Value(inst as u32)).is_none() {
        live[inst] = false;
    }
    words_of(view, inst, |word| {
        if let Word::Value(value) = word {
            live[value as usize] = true;
        }
    });
}

fn homes_hold(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let alloc = Alloc::global();
    let func = draw_func(tape)?;
    let view = func.view();
    let got = analyse_planted(view, &ARM64, alloc, plant)
        .map_err(|error| oracle(format!("the analysis refused a drawn function: {error}")))?;
    let (n, nb) = (view.len(), view.blocks());

    // Successors and use counts, read off the function.
    let mut succs: AVec<'static, AVec<'static, usize>> = Vec::new_in(alloc);
    let mut used = Vec::from_elem_in(0u32, n, alloc);
    for block in 0..nb {
        let mut list = Vec::new_in(alloc);
        words_of(view, view.terminator(Block(block as u32)), |word| {
            if let Word::Block(to) = word {
                list.push(to as usize);
            }
        });
        succs.push(list);
        for inst in view.block(Block(block as u32)) {
            words_of(view, inst, |word| {
                if let Word::Value(value) = word {
                    used[value as usize] += 1;
                }
            });
        }
    }

    // `order`: every block once, the entry first.
    let mut place = Vec::from_elem_in(usize::MAX, nb, alloc);
    for (at, &block) in got.order.iter().enumerate() {
        if place[block.index()] != usize::MAX {
            return Err(oracle(format!("block {} is in the order twice", block.0)));
        }
        place[block.index()] = at;
    }
    if got.order.len() != nb || got.order.first() != Some(&Block(0)) {
        return Err(oracle(format!("the order {:?} is not every block with the entry first", got.order.as_slice())));
    }

    // Liveness by the dataflow.
    let mut live_in: AVec<'static, AVec<'static, bool>> = Vec::new_in(alloc);
    for _ in 0..nb {
        live_in.push(Vec::from_elem_in(false, n, alloc));
    }
    let live_out = |live_in: &[AVec<'static, bool>], block: usize| {
        let mut out = Vec::from_elem_in(false, n, alloc);
        for &to in succs[block].iter() {
            for value in 0..n {
                out[value] = out[value] || live_in[to][value];
            }
        }
        out
    };
    for round in 0.. {
        if round > nb * n + 2 {
            return Err(Broke::Harness("the dataflow did not settle".to_string()));
        }
        let mut changed = false;
        for block in (0..nb).rev() {
            let mut live = live_out(&live_in, block);
            for inst in view.block(Block(block as u32)).rev() {
                step(view, inst, &mut live);
            }
            if live.as_slice() != live_in[block].as_slice() {
                live_in[block] = live;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // No two values live at one point share a register.
    for block in 0..nb {
        let range = view.block(Block(block as u32));
        let mut live = live_out(&live_in, block);
        for inst in range.clone().rev() {
            let after = Vec::from_slice_in(live.as_slice(), alloc);
            step(view, inst, &mut live);
            let mut held: [Option<usize>; 64] = [None; 64];
            let mut note = |value: usize| -> Result<(), Broke> {
                if let Home::Reg(reg) = got.home[value] {
                    match held[reg.0 as usize] {
                        Some(other) if other != value => {
                            return Err(oracle(format!(
                                "two live values share a register: v{other} and v{value} both in {reg:?} at instruction {inst}"
                            )));
                        }
                        _ => held[reg.0 as usize] = Some(value),
                    }
                }
                Ok(())
            };
            for value in 0..n {
                if after[value] || live[value] {
                    note(value)?;
                }
            }
            if inst == range.end - 1 {
                for &to in succs[block].iter() {
                    for param in view.params(Block(to as u32)) {
                        if used[param] > 0 {
                            note(param)?;
                        }
                    }
                }
            }
        }
    }

    // Nothing `Local` crosses a block boundary; a `Local` value's uses are in its block and its
    // last use is the latest of them; `next_use` is `NO_USE` exactly for a value with no use.
    for block in 0..nb {
        for value in 0..n {
            if live_in[block][value] && got.home[value] == Home::Local {
                return Err(oracle(format!("v{value} is live into block {block} but Local")));
            }
        }
    }
    for value in 0..n {
        if (used[value] == 0) != (got.next_use[value] == NO_USE) {
            return Err(oracle(format!("v{value} has {} uses but next_use {}", used[value], got.next_use[value])));
        }
    }

    // Back edges: exactly the edges to a block at or before their source in the order.
    let mut want: AVec<'static, (u32, u32)> = Vec::new_in(alloc);
    for block in 0..nb {
        for &to in succs[block].iter() {
            if place[to] <= place[block] {
                want.push((to as u32, block as u32));
            }
        }
    }
    want.sort_unstable();
    want.dedup();
    let mut have: AVec<'static, (u32, u32)> = Vec::new_in(alloc);
    for &(from, to) in got.back_edges.iter() {
        have.push((to.0, from.0));
    }
    have.sort_unstable();
    if want.as_slice() != have.as_slice() {
        return Err(oracle(format!("back edges {:?} where the order gives {:?}", have.as_slice(), want.as_slice())));
    }

    // Each header's map: every value live just after its parameters, at its home's place.
    let mut headers = 0;
    for (at, &(header, _)) in want.iter().enumerate() {
        if at > 0 && want[at - 1].0 == header {
            continue;
        }
        headers += 1;
        header_map(view, &got, header as usize, &live_out(&live_in, header as usize))?;
    }
    if got.loops.len() != headers {
        return Err(oracle(format!("{} loop rows for {headers} headers", got.loops.len())));
    }

    // Irreducible exactly when the graph less its dominator back edges keeps a cycle.
    let dom = dominators_of(&succs);
    let mut indegree = Vec::from_elem_in(0u32, nb, alloc);
    for block in 0..nb {
        for &to in succs[block].iter() {
            if !dom[block][to] {
                indegree[to] += 1;
            }
        }
    }
    let mut ready: AVec<'static, usize> = Vec::new_in(alloc);
    ready.extend((0..nb).filter(|&block| indegree[block] == 0));
    let mut done = 0;
    while let Some(block) = ready.pop() {
        done += 1;
        for &to in succs[block].iter() {
            if !dom[block][to] {
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.push(to);
                }
            }
        }
    }
    let irreducible = done < nb;
    if got.irreducible != irreducible {
        return Err(oracle(format!("irreducible is {} where the graph says {irreducible}", got.irreducible)));
    }

    // Register homes: allocatable, of the value's class, saved when callee-saved; none at all
    // in an irreducible function.
    for value in 0..n {
        let Home::Reg(reg) = got.home[value] else { continue };
        if irreducible {
            return Err(oracle(format!("v{value} has {reg:?} in an irreducible function")));
        }
        let ty = view.ty(Value(value as u32));
        let class = match ty {
            Ty::I64 | Ty::PTR => RegClass::Gpr,
            Ty::F64 => RegClass::Fpr,
            _ => return Err(oracle(format!("v{value} of a type with no register class has {reg:?}"))),
        };
        if reg.class() != class || !ARM64.allocatable.contains(reg) {
            return Err(oracle(format!("v{value} has {reg:?}, not an allocatable {class:?} register")));
        }
        if ARM64.callee_saved.contains(reg) && !got.saved.contains(reg) {
            return Err(oracle(format!("v{value} holds callee-saved {reg:?}, which `saved` lacks")));
        }
    }
    if got.saved.minus(ARM64.callee_saved).0 != 0 {
        return Err(oracle(format!("`saved` {:?} holds a register a call does not keep", got.saved)));
    }
    Ok(())
}

/// The header's map against the values live just after its parameters (`out` is its
/// live-out).
fn header_map(view: View<'_>, got: &Analysis<'_>, header: usize, out: &[bool]) -> Result<(), Broke> {
    let alloc = Alloc::global();
    let mut live = Vec::from_slice_in(out, alloc);
    let range = view.block(Block(header as u32));
    let body = view.params(Block(header as u32)).end;
    for inst in (body..range.end).rev() {
        step(view, inst, &mut live);
    }
    let Some(index) = got.loop_index(Block(header as u32)) else {
        return Err(oracle(format!("block {header} has back edges but no loop row")));
    };
    let row = got.loops[index as usize];
    let rows = &got.live[row.first as usize..(row.first + row.count) as usize];
    // blaze-tier record/form.rs `check`: "each header's live values by value", each once.
    if rows.windows(2).any(|pair| pair[0].value >= pair[1].value) {
        return Err(oracle(format!("header {header}'s rows are not sorted by value, each once: {rows:?}")));
    }
    for (value, &is_live) in live.iter().enumerate() {
        if !is_live {
            continue;
        }
        let found = rows.iter().find(|it| it.value as usize == value && !matches!(it.place, Place::Slot { .. }));
        let right = match (got.home[value], found.map(|it| it.place)) {
            (Home::Reg(reg), Some(Place::Reg(place))) => reg == place,
            (Home::Cell, Some(Place::Cell { .. })) => true,
            _ => false,
        };
        if !right {
            return Err(oracle(format!(
                "v{value} is live into header block {header} with home {:?} but its map says {:?}",
                got.home[value],
                found.map(|it| it.place)
            )));
        }
    }
    for it in rows {
        let extra = match it.place {
            Place::Slot { .. } => slot_of(view.len(), it.value).is_none_or(|slot| slot >= view.slots.len()),
            Place::Reg(_) | Place::Cell { .. } => !live.get(it.value as usize).copied().unwrap_or(false),
        };
        if extra {
            return Err(oracle(format!("header block {header}'s map holds {it:?}, which is not live there")));
        }
    }
    Ok(())
}

/// The invariant: no two values live at one point share a register across loops, back edges, and irreducibility.
/// No two values live at one point share a register; the loop maps, back edges, irreducibility
/// and classes hold (the module's list), on 512 drawn functions.
/// Expected: passes.
#[test]
fn homes_never_share_a_register_where_both_are_live() {
    check(Config::cases(512), |tape| homes_hold(tape, Plant::None));
}

/// The control: with the loop extension left out, a value live into a header loses its register
/// to a later value inside the loop, and the oracle sees two live values in one register.
/// Expected: passes, printing "plant NoLoopExtension: caught at case k of 512".
#[test]
fn the_oracle_catches_a_missing_loop_extension() {
    control(Config::cases(512), Plant::None, Plant::NoLoopExtension, "two live values share", homes_hold);
}
