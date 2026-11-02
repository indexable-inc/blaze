//! The walk from a call's target to the first user code against rustc's
//! `find_unreachable_code_from` as rustc writes it (copied below: recursive, over a list of
//! predecessors per block with an entry per edge), on hand-made and generated blocks:
//! storage markers and `Nop`s, `Goto` chains, blocks reached by more than one edge, imaginary
//! and unwind edges, `Return`, other terminators, cycles and unterminated blocks. The rules
//! over assignments and `FakeRead`s need places, which need a database: the fixtures of
//! `tests/mir/uninhabited.rs` hold them.

use blaze_mem::{Frame, Tag, frame};
use la_arena::RawIdx;

use super::*;
use crate::mir::{LocalId, Statement, Terminator};

// rustc's walk, as written there.

fn rustc_predecessors(blocks: &[FrameBlock<'_>]) -> Vec<Vec<BasicBlockId>> {
    let mut preds = vec![Vec::new(); blocks.len()];
    for (at, block) in blocks.iter().enumerate() {
        if let Some(terminator) = &block.terminator {
            for to in terminator.kind.all_successors() {
                preds[framed::block_index(to)].push(framed::block_id(at));
            }
        }
    }
    preds
}

fn rustc_find(
    blocks: &[FrameBlock<'_>],
    preds: &[Vec<BasicBlockId>],
    bb: BasicBlockId,
    depth: usize,
) -> Option<(MirSpan, &'static str)> {
    // rustc recurses without a bound; a cycle reached from nowhere would not end there.
    if depth == blocks.len() {
        return None;
    }
    let data = &blocks[framed::block_index(bb)];
    for statement in &data.statements {
        match &statement.kind {
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => continue,
            StatementKind::FakeRead(..) => return Some((statement.span, "definition")),
            _ => return Some((statement.span, "expression")),
        }
    }
    let terminator = data.terminator.as_ref()?;
    match terminator.kind {
        TerminatorKind::Goto { target } | TerminatorKind::Drop { target, .. } => {
            if preds[framed::block_index(target)][..] == [bb] {
                rustc_find(blocks, preds, target, depth + 1)
            } else {
                None
            }
        }
        TerminatorKind::Return => None,
        _ => Some((terminator.span, "expression")),
    }
}

// Blocks.

fn local(n: u32) -> LocalId {
    LocalId::from_raw(RawIdx::from_u32(n))
}

fn bb(n: usize) -> BasicBlockId {
    framed::block_id(n)
}

/// A span that names what it is on, to tell which statement the walk stopped at.
fn span(n: u32) -> MirSpan {
    MirSpan::Inlined(n)
}

fn block<'f>(
    frame: &'f Frame,
    statements: &[StatementKind],
    terminator: Option<TerminatorKind>,
    span_base: u32,
) -> FrameBlock<'f> {
    let mut kept = Vec::new_in(frame);
    for (at, kind) in statements.iter().enumerate() {
        kept.push(Statement { kind: kind.clone(), span: span(span_base + at as u32) });
    }
    FrameBlock {
        statements: kept,
        terminator: terminator.map(|kind| Terminator { span: span(span_base + 999), kind }),
        is_cleanup: false,
    }
}

fn walk(blocks: &[FrameBlock<'_>], frame: &Frame, from: BasicBlockId) -> Option<(MirSpan, &'static str)> {
    let units = framed::set_in(frame);
    let preds = predecessors(blocks, frame);
    let found = first_user_code(blocks, &units, &preds, from);
    let rustc = rustc_find(blocks, &rustc_predecessors(blocks), from, 0);
    assert_eq!(found, rustc, "from {from:?}");
    found
}

#[test]
fn skips_storage_markers_and_stops_at_the_first_statement() {
    frame::task(Tag::Mir, |frame| {
        let blocks = [block(
            frame,
            &[
                StatementKind::StorageDead(local(1)),
                StatementKind::StorageLive(local(2)),
                StatementKind::Nop,
            ],
            Some(TerminatorKind::Return),
            0,
        )];
        assert_eq!(walk(&blocks, frame, bb(0)), Some((span(2), "expression")));
    });
}

#[test]
fn follows_a_goto_into_a_block_only_it_leads_to() {
    frame::task(Tag::Mir, |frame| {
        let blocks = [
            block(frame, &[StatementKind::StorageDead(local(1))], Some(TerminatorKind::Goto { target: bb(1) }), 0),
            block(frame, &[], Some(TerminatorKind::Unreachable), 100),
        ];
        assert_eq!(walk(&blocks, frame, bb(0)), Some((span(1099), "expression")));
    });
}

#[test]
fn stops_at_a_block_another_edge_leads_to() {
    frame::task(Tag::Mir, |frame| {
        let blocks = [
            block(frame, &[], Some(TerminatorKind::Goto { target: bb(2) }), 0),
            block(frame, &[], Some(TerminatorKind::Goto { target: bb(2) }), 100),
            block(frame, &[StatementKind::Nop], Some(TerminatorKind::Return), 200),
        ];
        assert_eq!(walk(&blocks, frame, bb(0)), None);
    });
}

#[test]
fn an_imaginary_or_unwind_edge_is_an_edge() {
    frame::task(Tag::Mir, |frame| {
        let imaginary = [
            block(frame, &[], Some(TerminatorKind::Goto { target: bb(2) }), 0),
            block(
                frame,
                &[],
                Some(TerminatorKind::FalseEdge { real_target: bb(3), imaginary_target: bb(2) }),
                100,
            ),
            block(frame, &[StatementKind::Nop], Some(TerminatorKind::Return), 200),
            block(frame, &[], Some(TerminatorKind::Return), 300),
        ];
        assert_eq!(walk(&imaginary, frame, bb(0)), None);
        let unwind = [
            block(frame, &[], Some(TerminatorKind::Goto { target: bb(2) }), 0),
            block(
                frame,
                &[],
                Some(TerminatorKind::FalseUnwind { real_target: bb(3), unwind: Some(bb(2)) }),
                100,
            ),
            block(frame, &[StatementKind::Nop], Some(TerminatorKind::Return), 200),
            block(frame, &[], Some(TerminatorKind::Return), 300),
        ];
        assert_eq!(walk(&unwind, frame, bb(0)), None);
    });
}

#[test]
fn return_and_unterminated_give_nothing() {
    frame::task(Tag::Mir, |frame| {
        let blocks = [
            block(frame, &[StatementKind::StorageLive(local(1))], Some(TerminatorKind::Return), 0),
            block(frame, &[], None, 100),
        ];
        assert_eq!(walk(&blocks, frame, bb(0)), None);
        assert_eq!(walk(&blocks, frame, bb(1)), None);
    });
}

#[test]
fn a_cycle_reached_from_nowhere_ends() {
    frame::task(Tag::Mir, |frame| {
        let blocks = [
            block(frame, &[], Some(TerminatorKind::Goto { target: bb(1) }), 0),
            block(frame, &[StatementKind::StorageDead(local(1))], Some(TerminatorKind::Goto { target: bb(0) }), 100),
        ];
        assert_eq!(walk(&blocks, frame, bb(0)), None);
    });
}

// Generated blocks.

/// xorshift64*: the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn generated<'f>(rng: &mut Rng, frame: &'f Frame, count: usize) -> Vec<FrameBlock<'f>> {
    let mut blocks = Vec::with_capacity(count);
    for at in 0..count {
        let mut statements = Vec::new();
        for _ in 0..rng.below(4) {
            statements.push(match rng.below(10) {
                0..=3 => StatementKind::StorageLive(local(rng.below(8) as u32)),
                4..=7 => StatementKind::StorageDead(local(rng.below(8) as u32)),
                _ => StatementKind::Nop,
            });
        }
        let kind = rng.below(12);
        let mut to = || bb(rng.below(count as u64) as usize);
        let terminator = match kind {
            0..=4 => Some(TerminatorKind::Goto { target: to() }),
            5 => Some(TerminatorKind::FalseEdge { real_target: to(), imaginary_target: to() }),
            6 => Some(TerminatorKind::FalseUnwind { real_target: to(), unwind: Some(to()) }),
            7 | 8 => Some(TerminatorKind::Return),
            9 => Some(TerminatorKind::Unreachable),
            10 => Some(TerminatorKind::UnwindResume),
            _ => None,
        };
        blocks.push(block(frame, &statements, terminator, at as u32 * 1000));
    }
    blocks
}

#[test]
fn the_same_as_rustcs_walk_on_generated_blocks() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..2000 {
        frame::task(Tag::Mir, |frame| {
            let count = 1 + rng.below(12) as usize;
            let blocks = generated(&mut rng, frame, count);
            for from in 0..count {
                walk(&blocks, frame, bb(from));
            }
        });
    }
}
