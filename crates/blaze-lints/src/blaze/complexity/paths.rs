//! `path_count`'s count: the acyclic paths through a control-flow graph from its start to an
//! end, each loop taken at most once. Over MIR it is NPATH (Nejmeh, "NPATH: a measure of
//! execution path complexity and its applications", CACM 31(2), 1988) read off the graph rather
//! than the syntax: a sequence of branches multiplies, the arms of a branch add, a loop is its
//! body's paths plus the path that skips it, and an early exit (`return`, `?`, a call that does
//! not return) is one path that ends there. It is the number of tests a full path cover needs.
//!
//! How a loop is taken once (Ball and Larus, "Efficient path profiling", MICRO 1996, cut a loop
//! at its back edges the same way): a depth-first walk from the start finds the back edges (an
//! edge to a block still on the walk's stack); each is cut, and in its place the latch goes on
//! to the loop's exits, the blocks outside its natural loop (the blocks that reach the latch
//! without passing the header) that a block inside it goes to. So a `while` whose body has `b`
//! paths, followed by code with `r`, has `(b + 1) * r`. A block that cannot be reached
//! (`Unreachable`) ends no path. The count saturates at `u64::MAX`.

use std::alloc::Global;

/// A control-flow graph: each block's successors, by index, the start, and which blocks end no
/// path.
pub(crate) struct Cfg {
    pub(crate) successors: Vec<Vec<u32, Global>, Global>,
    /// A block that is never reached at run time (MIR's `Unreachable`).
    pub(crate) dead: Vec<bool, Global>,
    pub(crate) start: u32,
}

/// The acyclic paths of `cfg` from its start, each loop taken at most once.
pub(crate) fn count(cfg: &Cfg) -> u64 {
    let blocks = cfg.successors.len();
    if (cfg.start as usize) >= blocks {
        return 0;
    }
    let forward = forward_edges(cfg);
    // Each back edge's latch goes on to its loop's exits.
    let mut successors: Vec<Vec<u32, Global>, Global> = Vec::with_capacity_in(blocks, Global);
    successors.extend(forward.kept.iter().map(|it| it.clone()));
    let loops = natural_loops(&forward);
    let mut memo: Vec<Option<Vec<u32, Global>>, Global> = Vec::with_capacity_in(loops.len(), Global);
    memo.resize(loops.len(), None);
    for at in 0..loops.len() {
        let exits = exits_of(&forward, &loops, at, &mut memo, 0);
        for &latch in &loops[at].latches {
            let to = &mut successors[latch as usize];
            for &exit in &exits {
                if !to.contains(&exit) {
                    to.push(exit);
                }
            }
        }
    }
    paths_from(&successors, &cfg.dead, &forward.back, cfg.start)
}

/// The edges of a depth-first walk from the start: those kept (to a block not on the walk's
/// stack) and the back edges `(latch, header)`.
struct Forward {
    kept: Vec<Vec<u32, Global>, Global>,
    back: Vec<(u32, u32), Global>,
}

fn forward_edges(cfg: &Cfg) -> Forward {
    let blocks = cfg.successors.len();
    let mut kept: Vec<Vec<u32, Global>, Global> = Vec::with_capacity_in(blocks, Global);
    kept.resize_with(blocks, || Vec::new_in(Global));
    let mut back = Vec::new_in(Global);
    // 0 not seen, 1 on the stack, 2 done.
    let mut state = Vec::with_capacity_in(blocks, Global);
    state.resize(blocks, 0u8);
    // The walk's stack: a block and the index of its next successor.
    let mut stack: Vec<(u32, usize), Global> = Vec::new_in(Global);
    stack.push((cfg.start, 0));
    state[cfg.start as usize] = 1;
    while let Some(&mut (block, ref mut next)) = stack.last_mut() {
        let out = &cfg.successors[block as usize];
        if *next == out.len() {
            state[block as usize] = 2;
            stack.pop();
            continue;
        }
        let to = out[*next];
        *next += 1;
        if (to as usize) >= blocks {
            continue;
        }
        match state[to as usize] {
            1 => back.push((block, to)),
            0 => {
                kept[block as usize].push(to);
                state[to as usize] = 1;
                stack.push((to, 0));
            }
            _ => kept[block as usize].push(to),
        }
    }
    Forward { kept, back }
}

/// One natural loop: the back edges to one header (the body's end and each `continue`) make
/// one loop, the header and every block that reaches one of its latches without passing it.
struct Loop {
    header: u32,
    latches: Vec<u32, Global>,
    inside: Vec<bool, Global>,
}

/// The natural loops of `forward`'s back edges, one per header, by header.
fn natural_loops(forward: &Forward) -> Vec<Loop, Global> {
    let blocks = forward.kept.len();
    // Predecessors over every edge, back edges too: an inner loop's body reaches the outer
    // latch only through the inner back edge (`while { while { body } }`: the body goes back to
    // the inner header, which is the outer latch), and it is inside the outer loop all the same.
    // The walk back from a latch stops at the header, so it never leaves the loop.
    let mut preds: Vec<Vec<u32, Global>, Global> = Vec::with_capacity_in(blocks, Global);
    preds.resize_with(blocks, || Vec::new_in(Global));
    for (from, out) in forward.kept.iter().enumerate() {
        for &to in out {
            preds[to as usize].push(from as u32);
        }
    }
    for &(from, to) in &forward.back {
        preds[to as usize].push(from);
    }
    let mut headers: Vec<u32, Global> = Vec::new_in(Global);
    headers.extend(forward.back.iter().map(|&(_, header)| header));
    headers.sort_unstable();
    headers.dedup();
    let mut loops = Vec::with_capacity_in(headers.len(), Global);
    for header in headers {
        let mut latches: Vec<u32, Global> = Vec::new_in(Global);
        latches.extend(forward.back.iter().filter(|it| it.1 == header).map(|it| it.0));
        let mut inside = Vec::with_capacity_in(blocks, Global);
        inside.resize(blocks, false);
        inside[header as usize] = true;
        let mut work: Vec<u32, Global> = Vec::new_in(Global);
        for &latch in &latches {
            if !inside[latch as usize] {
                inside[latch as usize] = true;
                work.push(latch);
            }
        }
        while let Some(block) = work.pop() {
            for &pred in &preds[block as usize] {
                if !inside[pred as usize] {
                    inside[pred as usize] = true;
                    work.push(pred);
                }
            }
        }
        loops.push(Loop { header, latches, inside });
    }
    loops
}

/// Where control goes when the loop `at` is left: the blocks outside it a block inside goes to
/// over a kept edge, and, for a back edge from inside it to the header of a loop around it (an
/// inner loop's end that is also the outer one's, a `continue 'outer`), that loop's exits.
fn exits_of(
    forward: &Forward,
    loops: &[Loop],
    at: usize,
    memo: &mut [Option<Vec<u32, Global>>],
    depth: usize,
) -> Vec<u32, Global> {
    if let Some(known) = &memo[at] {
        return known.clone();
    }
    let mut exits: Vec<u32, Global> = Vec::new_in(Global);
    // Each step goes to a loop strictly around this one, so the depth is at most the loops'
    // count; the bound only stops a graph that is not reducible.
    if depth > loops.len() {
        return exits;
    }
    let this = &loops[at];
    for (block, out) in forward.kept.iter().enumerate() {
        if !this.inside[block] {
            continue;
        }
        for &to in out {
            if !this.inside[to as usize] && !exits.contains(&to) {
                exits.push(to);
            }
        }
    }
    for &(from, to) in &forward.back {
        if !this.inside[from as usize] || to == this.header || this.inside[to as usize] {
            continue;
        }
        let Ok(outer) = loops.binary_search_by_key(&to, |it| it.header) else { continue };
        for exit in exits_of(forward, loops, outer, memo, depth + 1) {
            if !exits.contains(&exit) {
                exits.push(exit);
            }
        }
    }
    memo[at] = Some(exits.clone());
    exits
}

/// The paths from `start` over `successors`, a graph with no cycle but where a cut edge's
/// exits close one (an irreducible graph): a block met again while its count is being made
/// ends the path there.
fn paths_from(
    successors: &[Vec<u32, Global>],
    dead: &[bool],
    back: &[(u32, u32)],
    start: u32,
) -> u64 {
    let blocks = successors.len();
    // A latch whose back edge was cut ends a path when its loop has no exit.
    let mut latch = Vec::with_capacity_in(blocks, Global);
    latch.resize(blocks, false);
    for &(from, _) in back {
        latch[from as usize] = true;
    }
    let mut memo: Vec<Option<u64>, Global> = Vec::with_capacity_in(blocks, Global);
    memo.resize(blocks, None);
    let mut open = Vec::with_capacity_in(blocks, Global);
    open.resize(blocks, false);
    let mut stack: Vec<(u32, usize, u64), Global> = Vec::new_in(Global);
    stack.push((start, 0, 0));
    open[start as usize] = true;
    while let Some(&mut (block, ref mut next, ref mut sum)) = stack.last_mut() {
        let at = block as usize;
        let out = &successors[at];
        if dead.get(at).copied().unwrap_or(false) {
            memo[at] = Some(0);
            open[at] = false;
            stack.pop();
            continue;
        }
        if *next == out.len() {
            let value = if out.is_empty() { 1 } else { *sum };
            let value = if out.is_empty() || !latch[at] { value } else { value.max(1) };
            memo[at] = Some(value);
            open[at] = false;
            stack.pop();
            if let Some((_, _, parent)) = stack.last_mut() {
                *parent = parent.saturating_add(value);
            }
            continue;
        }
        let to = out[*next];
        *next += 1;
        let to_at = to as usize;
        if let Some(known) = memo[to_at] {
            *sum = sum.saturating_add(known);
        } else if open[to_at] {
            *sum = sum.saturating_add(1);
        } else {
            open[to_at] = true;
            stack.push((to, 0, 0));
        }
    }
    memo[start as usize].unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(edges: &[&[u32]]) -> Cfg {
        let mut successors = Vec::new_in(Global);
        for out in edges {
            let mut it = Vec::new_in(Global);
            it.extend_from_slice(out);
            successors.push(it);
        }
        let mut dead = Vec::new_in(Global);
        dead.resize(edges.len(), false);
        Cfg { successors, dead, start: 0 }
    }

    /// NPATH (Nejmeh 1988, table 1): two `if`s in sequence, each with an `else`, are 2 * 2.
    #[test]
    fn branches_in_sequence_multiply() {
        // 0: if -> 1 | 2; both -> 3: if -> 4 | 5; both -> 6 (return)
        let it = cfg(&[&[1, 2], &[3], &[3], &[4, 5], &[6], &[6], &[]]);
        assert_eq!(count(&it), 4);
    }

    /// An early return is one path that ends: `a?; b?; c` is 3, not 2 * 2.
    #[test]
    fn an_early_exit_adds_one() {
        // 0: ? -> 1 (return) | 2: ? -> 3 (return) | 4 (return)
        let it = cfg(&[&[1, 2], &[], &[3, 4], &[], &[]]);
        assert_eq!(count(&it), 3);
    }

    /// NPATH of `while`: the body's paths plus one, times what follows.
    #[test]
    fn a_loop_is_taken_once() {
        // 0 -> 1 header; 1: cond -> 2 body | 5 after; 2: if -> 3 | 4; 3, 4 -> 1 (back);
        // 5: if -> 6 | 7; 6, 7 -> 8 (return)
        let it = cfg(&[&[1], &[2, 5], &[3, 4], &[1], &[1], &[6, 7], &[8], &[8], &[]]);
        // (2 + 1) * 2
        assert_eq!(count(&it), 6);
    }

    #[test]
    fn an_unreachable_block_ends_no_path() {
        let mut it = cfg(&[&[1, 2], &[], &[]]);
        it.dead[2] = true;
        assert_eq!(count(&it), 1);
    }

    /// A program of sequences, branches and loops: its NPATH by the formula, and its graph.
    #[derive(Debug, Clone)]
    enum Shape {
        Plain,
        If(Box<Shape>, Box<Shape>),
        While(Box<Shape>),
        Seq(Box<Shape>, Box<Shape>),
    }

    fn npath(shape: &Shape) -> u64 {
        match shape {
            Shape::Plain => 1,
            Shape::If(a, b) => npath(a) + npath(b),
            Shape::While(body) => npath(body) + 1,
            Shape::Seq(a, b) => npath(a) * npath(b),
        }
    }

    /// Adds `shape`'s blocks to `edges`, entered at the block it returns, leaving to `exit`.
    fn lower(shape: &Shape, exit: u32, edges: &mut Vec<Vec<u32>>) -> u32 {
        let new = |edges: &mut Vec<Vec<u32>>| {
            edges.push(Vec::new());
            (edges.len() - 1) as u32
        };
        match shape {
            Shape::Plain => {
                let at = new(edges);
                edges[at as usize].push(exit);
                at
            }
            Shape::If(a, b) => {
                let head = new(edges);
                let a = lower(a, exit, edges);
                let b = lower(b, exit, edges);
                edges[head as usize].extend([a, b]);
                head
            }
            Shape::While(body) => {
                let head = new(edges);
                let body = lower(body, head, edges);
                edges[head as usize].extend([body, exit]);
                head
            }
            Shape::Seq(a, b) => {
                let b = lower(b, exit, edges);
                lower(a, b, edges)
            }
        }
    }

    fn draw_shape(tc: &hegel::TestCase, depth: u32) -> Shape {
        use hegel::generators as gs;
        let kind: u8 = tc.draw(gs::integers::<u8>().max_value(if depth == 0 { 0 } else { 3 }));
        match kind {
            0 => Shape::Plain,
            1 => Shape::If(Box::new(draw_shape(tc, depth - 1)), Box::new(draw_shape(tc, depth - 1))),
            2 => Shape::While(Box::new(draw_shape(tc, depth - 1))),
            _ => Shape::Seq(Box::new(draw_shape(tc, depth - 1)), Box::new(draw_shape(tc, depth - 1))),
        }
    }

    /// The invariant: on a structured program the count is NPATH's, and numbering the blocks
    /// otherwise (the order items or blocks are laid out in) changes nothing.
    #[hegel::test]
    fn the_count_is_npath_whatever_the_block_order(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let shape = draw_shape(&tc, 4);
        let mut edges = vec![Vec::new()];
        let entry = lower(&shape, 0, &mut edges);
        // Block 0 is the return; a start block goes to the entry.
        edges.push(vec![entry]);
        let start = (edges.len() - 1) as u32;
        let blocks = edges.len();
        let order: Vec<u32> = tc.draw(gs::permutations((0..blocks as u32).collect::<Vec<u32>>()));
        let mut successors = Vec::new_in(Global);
        successors.resize_with(blocks, || Vec::new_in(Global));
        for (from, out) in edges.iter().enumerate() {
            for &to in out {
                successors[order[from] as usize].push(order[to as usize]);
            }
        }
        let mut dead = Vec::new_in(Global);
        dead.resize(blocks, false);
        let it = Cfg { successors, dead, start: order[start as usize] };
        assert_eq!(count(&it), npath(&shape), "{shape:?}");
    }
}
