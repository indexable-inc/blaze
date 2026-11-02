# fastcompile-inproc

The requests of fastcompile-inproc are in `v2/requests/fastcompile.md`, under the headings
`fastcompile-inproc 1` (link) to `fastcompile-inproc 4` (coordinator), as the lane's rules ask.

## dedup: the fence model's reverse edges onto `blaze_radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_radix::Csr<T>` (reached as
`blaze_ids::radix::Csr`; `crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into
compressed sparse rows: `Csr::build(rows, pairs)` / `csr.rebuild(rows, pairs, &mut cursors)` walk a
cloneable iterator of `(row: u32, item)` twice (count, then place at each row's cursor), with
`row(r) -> &[T]`, `range(r)`, `items()`, `into_parts() -> (starts, items)` (`starts` has
`rows + 1` entries). A warm `rebuild` allocates nothing. Its tests have the old copies as the
proptest reference.

- `crates/blaze-inproc/Cargo.toml`: add

```toml
[dev-dependencies]
blaze-radix.workspace = true
```

  (test-only: `fence/model.rs` is `#[cfg(test)]`; `blaze-radix` has no dependencies).
- `crates/blaze-inproc/src/fence/model.rs`: replace from `let mut starts = vec![0u32; count + 1];`
  through the `for &(source, to) in &edges { from[...] = source; ... }` loop with

```rust
    let sources = blaze_radix::Csr::build(count, edges.iter().map(|&(source, to)| (to, source)));
```

  and `for &source in &from[starts[at as usize] as usize..starts[at as usize + 1] as usize] {`
  becomes `for &source in sources.row(at as usize) {`.

Why: one counting sort in the tree.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: bits: blaze-inproc the fence table's used entries onto `blaze_ids::bits`

From `dedup-bits-edge` (report `/tmp/blaze-lanes/v2-dedup-bits-edge.md`). `Fences::used`, a
`[u64; ENTRIES / 64]` set with its scans by hand, becomes a `blaze_ids::bits::BitArray` (the
same 32 inline bytes); `high` stays beside it. `blaze-ids` is already a dependency of the crate.
The free entry found is the same (`first_absent`: the first word not all ones, its
`trailing_ones`), and `high` after a clear is one past the largest entry still used, which is
where the old walk down from `high` stopped since no entry at or past `high` is used. The
crate's own `Words` trait is not touched (only `BitArray` is imported).

`crates/blaze-inproc/src/fence.rs`, imports, before / after:

```rust
use std::sync::atomic::{AtomicU64, Ordering, fence};
```

```rust
use std::sync::atomic::{AtomicU64, Ordering, fence};

use blaze_ids::bits::BitArray;
```

The field (line 121):

```rust
    used: [u64; ENTRIES / 64],
```

```rust
    used: BitArray<{ ENTRIES / 64 }>,
```

In `Fences::new` (line 131):

```rust
        Fences { words, used: [0; ENTRIES / 64], high: 0 }
```

```rust
        Fences { words, used: Default::default(), high: 0 }
```

In `Fences::post` (lines 145-148):

```rust
        let index = self.used.iter().enumerate().find_map(|(word, bits)| {
            (*bits != u64::MAX).then(|| word * 64 + bits.trailing_ones() as usize)
        })?;
        self.used[index / 64] |= 1 << (index % 64);
```

```rust
        let index = self.used.first_absent()?;
        self.used.insert(index);
```

In `Fences::clear` (lines 179-182):

```rust
        self.used[index / 64] &= !(1 << (index % 64));
        while self.high > 0 && self.used[(self.high - 1) / 64] & (1 << ((self.high - 1) % 64)) == 0 {
            self.high -= 1;
        }
```

```rust
        self.used.remove(index);
        self.high = self.used.last().map_or(0, |it| it + 1);
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).
