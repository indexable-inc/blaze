use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
};

use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// The spec: `blaze-link`'s `Csr::build` (`native.rs`) as it was before it moved here, the
/// plainest of the copies (count, prefix sum, place with the starts as cursors, shift back).
fn reference(rows: usize, pairs: &[(u32, u32)]) -> (Vec<u32>, Vec<u32>) {
    let mut at = vec![0u32; rows + 1];
    for (row, _) in pairs {
        at[*row as usize + 1] += 1;
    }
    for row in 0..rows {
        at[row + 1] += at[row];
    }
    let mut items = vec![0; pairs.len()];
    for (row, item) in pairs {
        let slot = &mut at[*row as usize];
        items[*slot as usize] = *item;
        *slot += 1;
    }
    for row in (1..=rows).rev() {
        at[row] = at[row - 1];
    }
    at[0] = 0;
    (at, items)
}

/// Rows and pairs with every row below them, some rows empty and some with many pairs.
#[hegel::composite]
fn rows_and_pairs(tc: &TestCase) -> (usize, Vec<(u32, u32)>) {
    let rows: usize = tc.draw(gs::integers().min_value(1).max_value(79));
    let pairs = tc.draw(
        gs::vecs(gs::tuples!(gs::integers::<u32>().max_value(rows as u32 - 1), gs::integers::<u32>()))
            .max_size(399),
    );
    (rows, pairs)
}

#[hegel::test]
fn build_is_the_reference_counting_sort(tc: TestCase) {
    let (rows, pairs) = tc.draw(rows_and_pairs());
    let csr = Csr::build(rows, pairs.iter().copied());
    let (starts, items) = reference(rows, &pairs);
    assert_eq!(csr.starts(), &starts[..]);
    assert_eq!(csr.items(), &items[..]);
    assert_eq!(csr.rows(), rows);
    for row in 0..rows {
        let want: Vec<u32> = pairs.iter().filter(|it| it.0 as usize == row).map(|it| it.1).collect();
        assert_eq!(csr.row(row), &want[..]);
        assert_eq!(csr.get(row), Some(&want[..]));
    }
    assert_eq!(csr.get(rows), None);
}

#[hegel::test]
fn a_rebuild_is_a_build(tc: TestCase) {
    let (rows, pairs) = tc.draw(rows_and_pairs());
    let (rows2, pairs2) = tc.draw(rows_and_pairs());
    let mut csr = Csr::build(rows, pairs.iter().copied());
    let mut cursors = Vec::new();
    csr.rebuild(rows2, pairs2.iter().copied(), &mut cursors);
    assert_eq!(csr, Csr::build(rows2, pairs2.iter().copied()));
}

#[hegel::test]
fn a_graph_is_its_edges_sorted(tc: TestCase) {
    let (nodes, pairs) = tc.draw(rows_and_pairs());
    let edges: Vec<(u32, u32)> = pairs.iter().map(|&(s, t)| (s, t % nodes as u32)).collect();
    let graph = Csr::graph(nodes, edges.iter().copied());
    let mut sorted = edges.clone();
    sorted.sort_unstable();
    let (starts, _) = reference(nodes, &sorted);
    assert_eq!(graph.starts(), &starts[..]);
    assert!(graph.items().iter().eq(sorted.iter().map(|it| &it.1)));

    let mut set = graph;
    set.dedup_rows();
    sorted.dedup();
    let (starts, _) = reference(nodes, &sorted);
    assert_eq!(set.starts(), &starts[..]);
    assert!(set.items().iter().eq(sorted.iter().map(|it| &it.1)));
}

#[test]
fn no_rows_and_no_pairs() {
    let none: Csr<u8> = Csr::new();
    assert_eq!((none.rows(), none.len(), none.starts()), (0, 0, &[][..]));
    assert_eq!(none.get(0), None);
    let empty = Csr::<u8>::build(0, std::iter::empty());
    assert_eq!((empty.rows(), empty.starts()), (0, &[0][..]));
    let blank = Csr::<u8>::build(3, std::iter::empty());
    assert_eq!(blank.starts(), &[0, 0, 0, 0]);
    assert_eq!(blank.row(2), &[] as &[u8]);
    let (starts, items) = blank.into_parts();
    assert_eq!(Csr::from_parts(starts, items).rows(), 3);
}

#[test]
fn items_that_are_not_copy_are_placed_once() {
    let words = ["b", "a", "c", "d"];
    let csr = Csr::build(2, words.iter().enumerate().map(|(at, it)| ((at % 2) as u32, it.to_string())));
    assert_eq!(csr.row(0), ["b", "c"]);
    assert_eq!(csr.row(1), ["a", "d"]);
}

#[test]
#[should_panic(expected = "is not below its 2 rows")]
fn a_row_past_the_rows_is_a_bug() {
    Csr::build(2, [(0, ()), (2, ())]);
}

#[test]
#[should_panic(expected = "not a prefix sum")]
fn parts_that_are_not_a_csr_are_refused() {
    Csr::from_parts(vec![0, 2, 1], vec![1u8]);
}

/// Pairs whose second walk moves one pair to another row: each would leave a place
/// unwritten, and each stops with a panic instead.
#[test]
fn a_second_walk_with_other_rows_panics() {
    for (first, second) in [(0u32, 1u32), (1, 0), (2, 0)] {
        let walks = Cell::new(0usize);
        let pairs = (0..6u32).map(|at| {
            if at == 0 {
                walks.set(walks.get() + 1);
            }
            let row = if at == 3 { if walks.get() == 1 { first } else { second } } else { at % 3 };
            (row, at)
        });
        let caught = catch_unwind(AssertUnwindSafe(|| Csr::build(3, pairs)));
        assert!(caught.is_err(), "{first} then {second}");
    }
    let walks = Cell::new(0usize);
    let shorter = (0..6u32)
        .inspect(|&at| {
            if at == 0 {
                walks.set(walks.get() + 1);
            }
        })
        .take_while(|&at| walks.get() == 1 || at < 5)
        .map(|at| (at % 2, at));
    assert!(catch_unwind(AssertUnwindSafe(|| Csr::build(2, shorter))).is_err());
}

#[test]
fn dedup_keeps_each_rows_first_of_a_run() {
    let mut csr = Csr::build(3, [(0, 1u32), (0, 1), (0, 2), (2, 2), (2, 2), (0, 1)]);
    csr.dedup_rows();
    assert_eq!(csr.row(0), [1, 2, 1]);
    assert_eq!(csr.row(1), [] as [u32; 0]);
    assert_eq!(csr.row(2), [2]);
    assert_eq!(csr.len(), 4);
}

/// Once its columns and cursors have the room, a rebuild (and a graph's) allocates nothing.
#[test]
fn a_warm_rebuild_allocates_nothing() {
    let pairs: Vec<(u32, u32)> = (0..10_000u32).map(|at| (at.wrapping_mul(2_654_435_761) % 500, at)).collect();
    let (mut csr, mut cursors) = (Csr::new(), Vec::new());
    csr.rebuild(500, pairs.iter().copied(), &mut cursors);
    let (mut graph, mut by_target) = (Csr::new(), Csr::new());
    graph.rebuild_graph(500, pairs.iter().map(|&(s, t)| (s, t % 500)), &mut by_target, &mut cursors);
    blaze_alloc_count::assert_at_most(0, "a warm CSR rebuild", || {
        for round in 0..20u32 {
            csr.rebuild(500, pairs.iter().map(|&(row, it)| ((row + round) % 500, it)), &mut cursors);
            graph.rebuild_graph(500, pairs.iter().map(|&(s, t)| (s, (t + round) % 500)), &mut by_target, &mut cursors);
        }
    });
    assert_eq!(csr.len(), pairs.len());
    assert_eq!(graph.len(), pairs.len());
}

/// `System`, past the counted global allocator, with a count of what it is asked for.
#[derive(Clone, Copy)]
struct Scratch<'a>(&'a Cell<usize>);

// SAFETY: every call is `System`'s, which upholds `Allocator`'s contract; the count is a side
// table that no block depends on.
unsafe impl Allocator for Scratch<'_> {
    fn allocate(&self, layout: std::alloc::Layout) -> Result<std::ptr::NonNull<[u8]>, std::alloc::AllocError> {
        self.0.set(self.0.get() + 1);
        std::alloc::System.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: std::ptr::NonNull<u8>, layout: std::alloc::Layout) {
        // SAFETY: `ptr` came from `allocate` above, so from `System`, with this `layout`.
        unsafe { std::alloc::System.deallocate(ptr, layout) }
    }
}

/// `build_in` is `build`, with the cursors (one block) in the scratch and only the two columns
/// on the heap.
#[test]
fn build_in_puts_the_cursors_in_the_scratch() {
    let pairs: Vec<(u32, u32)> = (0..2_000u32).map(|at| (at.wrapping_mul(2_654_435_761) % 300, at)).collect();
    let asked = Cell::new(0);
    let csr = blaze_alloc_count::assert_at_most(2, "a CSR built with scratch cursors", || {
        Csr::build_in(300, pairs.iter().copied(), Scratch(&asked))
    });
    assert_eq!(asked.get(), 1);
    assert_eq!(csr, Csr::build(300, pairs.iter().copied()));
}

/// A CSR and a graph with their columns, scratch and cursors all in an allocator of their own
/// are the global ones, and ask the heap for nothing.
#[test]
fn a_csr_in_an_allocator_is_the_global_one() {
    let pairs: Vec<(u32, u32)> = (0..2_000u32).map(|at| (at.wrapping_mul(2_654_435_761) % 300, at)).collect();
    let asked = Cell::new(0);
    let (csr, graph) = blaze_alloc_count::assert_at_most(0, "a CSR in its own allocator", || {
        let mut cursors = Vec::new_in(Scratch(&asked));
        let mut csr = Csr::new_in(Scratch(&asked));
        csr.rebuild(300, pairs.iter().copied(), &mut cursors);
        let (mut graph, mut by_target) = (Csr::new_in(Scratch(&asked)), Csr::new_in(Scratch(&asked)));
        graph.rebuild_graph(300, pairs.iter().map(|&(s, t)| (s, t % 300)), &mut by_target, &mut cursors);
        graph.dedup_rows();
        (csr, graph)
    });
    assert!(asked.get() >= 5, "the columns, by-target columns and cursors were not in the scratch");
    assert_eq!(csr, Csr::build(300, pairs.iter().copied()));
    let mut global = Csr::graph(300, pairs.iter().map(|&(s, t)| (s, t % 300)));
    global.dedup_rows();
    assert_eq!(graph, global);
    let (starts, items) = csr.into_parts();
    assert_eq!(Csr::from_parts(starts, items), Csr::build(300, pairs.iter().copied()));
}
