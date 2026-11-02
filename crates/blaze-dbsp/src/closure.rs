use std::hash::Hash;

use blaze_hash::HashSet;

/// The DRed over-delete closure: from `seeds`, every node `step` reaches, each once, in BFS
/// order; `out` is cleared first. `step(n, next)` appends `n`'s successors to `next`.
///
/// Makes its visited set for the call; a maintainer that runs it per edit keeps a [`Closure`].
pub fn closure<N: Copy + Eq + Hash>(
    seeds: impl IntoIterator<Item = N>,
    step: impl FnMut(N, &mut Vec<N>),
    out: &mut Vec<N>,
) {
    Closure::new().run(seeds, step, out);
}

/// [`closure`] with its visited set and successor buffer kept between calls: a warm run that
/// reaches no more nodes than an earlier one allocates nothing.
#[derive(Debug)]
pub struct Closure<N> {
    seen: HashSet<N>,
    next: Vec<N>,
}

impl<N: Copy + Eq + Hash> Default for Closure<N> {
    fn default() -> Self {
        Closure::new()
    }
}

impl<N: Copy + Eq + Hash> Closure<N> {
    pub fn new() -> Self {
        Closure { seen: HashSet::default(), next: Vec::new() } // alloc: setup: empty buffers, grown once and kept
    }

    /// `O(nodes reached + edges stepped)`, one set probe per edge.
    pub fn run(
        &mut self,
        seeds: impl IntoIterator<Item = N>,
        mut step: impl FnMut(N, &mut Vec<N>),
        out: &mut Vec<N>,
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
