# Requests to `dedup-codec-rng`

## From speed-link-edge-hot-proof: `crates/blaze-v2-proof/src/lib.rs` hot_lint findings (20)

You hold `lib.rs` (the `Rng` move); every other file in `blaze-v2-proof` is done. Please apply
these with your `Rng` change, or tell me (window `speed-link-edge-hot-proof-next1`) once you are
DONE and I will. `PYTHONPATH=python/typed-input/src python3 /tmp/sle_lint.py crates/blaze-v2-proof/src`
should then print 0. Behavior and bytes are unchanged.

1. `TraceBuilder::finish`, the range `.clone()`s (lines 120-121 and 154-156): index by two
   `usize`s instead of cloning a `Range`.

   ```rust
       let (from, to) = (row.from as usize, (row.from + row.len) as usize);
       for (id, edge) in read_ids[from..to].iter_mut().zip(&self.edges[from..to]) {
   ```

   and

   ```rust
                   let (from, to) = (row.from as usize, (row.from + row.len) as usize);
                   trace.reads.extend(read_ids[from..to].iter().map(|&read| rank[read as usize]));
                   trace.saw.extend(self.edges[from..to].iter().map(|e| e.saw));
   ```

2. `TraceBuilder::finish`, each buffer is sized once a trace. Put these on the line above:
   - `let mut read_ids = vec![NONE; ...]`: `// alloc: once a trace, sized to its edges: the key ID each edge read.`
   - `let mut order: Vec<u32> = ...collect()`: `// alloc: once a trace, the sort's permutation of the distinct keys.`
   - `let mut rank = vec![0u32; ...]`: `// alloc: once a trace, each key ID's sorted index.`
   - `keys: order.iter()...collect(),`: `// alloc: the trace's key column, once a trace.`
   - `fingerprints: vec![Fingerprint(0); n],`: `// alloc: the trace's fingerprint column, once a trace.`

3. `KeyTable::with_capacity` (line 179), above the `KeyTable { ... }` line:
   `// alloc: once a trace, the table's slots, sized for its keys.`

4. `Trace::edges` (line 260-261), called per key in every join: no range clone.

   ```rust
       let (from, to) = (self.edges_from[i as usize] as usize, self.edges_from[i as usize + 1] as usize);
       self.reads[from..to].iter().copied().zip(self.saw[from..to].iter().copied())
   ```

5. The answers, unallocated while empty:
   - `under`, `let mut stale = Vec::new();`: `// alloc: the answer; unallocated while nothing is stale.`
   - `over`, `let mut needless = Vec::new();`: `// alloc: the answer; unallocated while nothing reran for nothing.`
   - `apart`, `let mut out = Vec::new();`: `// alloc: the answer; unallocated while the maps agree.`
   - above each of the three `out.push(Apart::...)` lines: `// clone: the answer owns the keys and values where the maps differ, and only those.`

6. Derives on big types (speed-link-edge-hot-shared.md part 1), the line above:
   - `Trace`'s `#[derive(Clone, ...)]`: `// clone: tests only (join.rs's tests mutate a copy of a trace); nothing in the library clones one.`
   - `Stale`'s derive: `// clone: no heap, keys and fingerprints only; nameres's `Finding::Stale` carries one.`

7. `sample` (yours to rewrite with `Rng`; whatever `Rng` becomes, keep this shape): drop the
   second `to_vec` and the `into_iter`.

   ```rust
   pub fn sample<T: Clone>(items: &[T], n: usize, seed: u64) -> Vec<T> {
       if n >= items.len() {
           // alloc: the answer, once a sample: every item when there are no more.
           return items.to_vec();
       }
       let mut rng = Rng::new(seed);
       // alloc: once a sample, the partial shuffle's indices.
       let mut at: Vec<usize> = (0..items.len()).collect();
       for i in 0..n {
           let j = i + rng.below((items.len() - i) as u64) as usize;
           at.swap(i, j);
       }
       at.truncate(n);
       radix::sort(&mut at);
       // clone: the answer, the chosen items, once a sample.
       at.iter().map(|&i| items[i].clone()).collect()
   }
   ```

Status: handed back. dedup-codec-rng is DONE with `lib.rs` (the `Rng` move is in: `pub use blaze_ids::rng::SplitMix64 as Rng;`, 20:59), so speed-link-edge-hot-proof-next1 applies all 7 items, including the `sample` shape (it works with the re-exported `Rng` as written).

Status: done by speed-link-edge-hot-proof (21:10), after dedup-bits-next1 released `lib.rs` too; the `Rng` re-export kept. Lint 0.

> forwarded to dedup-codec-rng (holds `blaze-v2-proof/src/lib.rs`; speed-link-edge-hot-proof-next1 applies them once it is DONE) (requests, pass 19; told by tmux).
