use core::{alloc::Allocator, hash::Hash};

use blaze_hash::{FixedState, Set, SetOpts};
use blaze_own::Vec as OwnVec;

/// The DRed over-delete closure: from `seeds`, every node `step` reaches, each once, in BFS
/// order; `out` is cleared first. `step(n, next)` appends `n`'s successors to `next`.
///
/// Makes its visited set for the call in `alloc`; a maintainer that runs it per edit keeps a
/// [`Closure`].
pub fn closure_in<N: Copy + Eq + Hash, A: Allocator + Clone, B: Allocator>(
    seeds: impl IntoIterator<Item = N>,
    step: impl FnMut(N, &mut OwnVec<N, A>),
    out: &mut OwnVec<N, B>,
    alloc: A,
) {
    Closure::new_in(alloc).run(seeds, step, out);
}

/// [`closure_in`] with its visited set and successor buffer kept between calls: a warm run that
/// reaches no more nodes than an earlier one allocates nothing.
#[derive(Debug)]
pub struct Closure<N, A: Allocator> {
    seen: Set<N, FixedState, A>,
    next: OwnVec<N, A>,
}

impl<N: Copy + Eq + Hash, A: Allocator + Clone> Closure<N, A> {
    pub fn new_in(alloc: A) -> Self {
        Closure {
            seen: Set::new(SetOpts { hasher: FixedState, alloc: alloc.clone(), capacity: None }),
            next: OwnVec::new_in(alloc),
        }
    }
}

impl<N: Copy + Eq + Hash, A: Allocator> Closure<N, A> {
    /// `O(nodes reached + edges stepped)`, one set probe per edge.
    pub fn run<B: Allocator>(
        &mut self,
        seeds: impl IntoIterator<Item = N>,
        mut step: impl FnMut(N, &mut OwnVec<N, A>),
        out: &mut OwnVec<N, B>,
    ) {
        out.clear();
        self.seen.clear();
        for seed in seeds {
            if self.seen.insert(seed) {
                out.push(seed);
            }
        }
        let mut at = 0;
        while at < out.len() {
            self.next.clear();
            step(out[at], &mut self.next);
            for &node in &self.next {
                if self.seen.insert(node) {
                    out.push(node);
                }
            }
            at += 1;
        }
    }
}
