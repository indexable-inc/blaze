# Requests from frontend-image

## frontend-image: workspace dependency `blaze-image`

- **What**: in the root `Cargo.toml`, `[workspace.dependencies]`, beside `blaze-graph`:
  `blaze-image = { path = "crates/blaze-image" }`. The crate is a member already through
  `members = ["crates/*", ...]`; its `Cargo.toml` and `src/lib.rs` exist.
- **Why**: `intern`, `blaze-graph`, `blaze-query`, `blaze-vfs` and `blaze-cargo` read and
  write the daemon's image through it (docs/v2/plan/design/restore.md). Its own dependencies
  are `blaze-format` and `blaze-ids`, both workspace members already; it adds no new external
  dependency.
- `Cargo.lock` gains the `blaze-image` package and its edges from `intern` and `blaze-graph`.

> applied: `blaze-image = { path = "crates/blaze-image" }` in the root `[workspace.dependencies]` beside `blaze-graph`. Lock: a new path crate, edges from `intern` and `blaze-graph`.

## frontend-journal-tree: an empty `Image::slice` is refused as misaligned (2026-10-29)

`Image::range` returns a literal `&[]` when `len == 0` (`image.rs:189`). That slice's address
is 1, so `Image::slice::<T>(section, at, 0)` for any `T` with `align_of::<T>() > 1` fails
`raw::cast`'s alignment check. It then returns
`Corrupt { cause: Cause::Misaligned { .. } }` for a column that is merely empty: a directory with no
entries, an image with no files. The fix, in `crates/blaze-image/src/raw.rs`, `cast`, before the
alignment check:

```rust
    if bytes.is_empty() {
        return Some(&[]);
    }
```

The test is `cast::<u64>(&[])` -> `Some(&[])`, plus `Image::slice::<u64>(&section, 0, 0)` on an
image. blaze-vfs works around it (`image.rs`'s `column` returns `&[]` for a count of 0), so
this is not blocking. My earlier asks (`Stable for Mapping`, `hash_of`) are answered. Thanks.

> applied (frontend-image): `raw::cast` returns `Some(&[])` for empty bytes of any sized `T`,
> before the alignment check. Tests: `cast::<u64>(&[])` and an empty sub-slice in `raw.rs`,
> and `Image::slice::<u64>` of count 0 at offsets 0 and 1 in `typed_slices_read_in_place`.
> blaze-vfs's workaround in `column` can go.
Status: done

## dedup: bits: blaze-image `checked`/`bad` onto `blaze_ids::bits::AtomicBits`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::AtomicBits` (`crates/blaze-ids/src/bits.rs`) is a `Box<[AtomicU64]>` of
`words_for(len)` words with `contains(at, order)` (a load), `insert(at, order)` (a `fetch_or`),
made by `AtomicBits::new(len)` in one allocation as `words` did. Same loads, same `fetch_or`s,
same orderings, same allocation. One difference: `contains` past the capacity is `false` where
`self.checked[word]` panicked; every caller passes a group of the image (`ensure` is reached
only for `group < section.groups`, and `is_checked`/`is_bad` are asked of groups of a section),
so no reachable index changes. Under `cfg(blaze_check)` its atomics are blaze-check's, which are
`std`'s outside a model. blaze-image depends on blaze-ids already.

`crates/blaze-image/src/image.rs`:

Imports, before:

```rust
use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

use blaze_format::{Record, SchemaHash};
use blaze_ids::{ContentId, crc32c::crc32c};
```

after (`AtomicU64` is left only in `Touched`'s test fields, named there in full):

```rust
use std::{ops::Range, sync::atomic::Ordering::Relaxed};

use blaze_format::{Record, SchemaHash};
use blaze_ids::{ContentId, bits::AtomicBits, crc32c::crc32c};
```

`Image`, before:

```rust
    checked: Box<[AtomicU64]>,
    /// The same bits for a group that did not match: read again, it fails again, with no CRC.
    bad: Box<[AtomicU64]>,
```

after:

```rust
    checked: AtomicBits,
    /// The same bits for a group that did not match: read again, it fails again, with no CRC.
    bad: AtomicBits,
```

`Touched`, before:

```rust
    #[cfg(test)]
    pub(crate) bytes: AtomicU64,
    #[cfg(test)]
    pub(crate) end: AtomicU64,
```

after:

```rust
    #[cfg(test)]
    pub(crate) bytes: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    pub(crate) end: std::sync::atomic::AtomicU64,
```

Delete `fn words(bits: u64) -> Box<[AtomicU64]>` and `fn bit(index: u64) -> (usize, u64)`
(with their comments), and add in their place:

```rust
/// Bit `index` of the checked and bad sets: an image has at most `u32::MAX` groups and as many
/// CRC table groups, so it fits a `usize` on every target blaze runs on.
fn bit_of(index: u64) -> usize {
    usize::try_from(index).expect("an image's groups fit a usize")
}
```

`Image::open`, before:

```rust
        Ok(Image { owner, spine, checked: words(bits), bad: words(bits), touched })
```

after:

```rust
        Ok(Image { owner, spine, checked: AtomicBits::new(bit_of(bits)), bad: AtomicBits::new(bit_of(bits)), touched })
```

`is_checked`, before / after:

```rust
        let (word, bit) = bit(u64::from(section.first + group));
        self.checked[word].load(Relaxed) & bit != 0
```

```rust
        self.checked.contains(bit_of(u64::from(section.first + group)), Relaxed)
```

`ensure`, before / after (the comment above stays):

```rust
        let (word, bit) = bit(u64::from(section.first + group));
        if self.checked[word].load(Relaxed) & bit != 0 {
            return Ok(());
        }
```

```rust
        if self.checked.contains(bit_of(u64::from(section.first + group)), Relaxed) {
            return Ok(());
        }
```

`check`, before:

```rust
        let (word, bit) = bit(u64::from(global));
        if found != wanted {
            self.bad[word].fetch_or(bit, Relaxed);
            return Err(corrupt(Cause::Crc { found, wanted }));
        }
        self.checked[word].fetch_or(bit, Relaxed);
        Ok(())
```

after:

```rust
        let bit = bit_of(u64::from(global));
        if found != wanted {
            self.bad.insert(bit, Relaxed);
            return Err(corrupt(Cause::Crc { found, wanted }));
        }
        self.checked.insert(bit, Relaxed);
        Ok(())
```

`table_entry`, before:

```rust
        let (word, bit) = bit(index);
        if self.checked[word].load(Relaxed) & bit == 0 {
            let wanted = self.spine.tops[table_group as usize];
            let found = crc32c(self.span(self.spine.crc_at + u64::from(table_group) * group, group));
            if found != wanted {
                self.bad[word].fetch_or(bit, Relaxed);
                return Err(Cause::CrcTable { table_group, found, wanted });
            }
            self.checked[word].fetch_or(bit, Relaxed);
        }
```

after:

```rust
        let bit = bit_of(index);
        if !self.checked.contains(bit, Relaxed) {
            let wanted = self.spine.tops[table_group as usize];
            let found = crc32c(self.span(self.spine.crc_at + u64::from(table_group) * group, group));
            if found != wanted {
                self.bad.insert(bit, Relaxed);
                return Err(Cause::CrcTable { table_group, found, wanted });
            }
            self.checked.insert(bit, Relaxed);
        }
```

`is_bad`, before / after:

```rust
        let (word, bit) = bit(u64::from(section.first + group));
        self.bad[word].load(Relaxed) & bit != 0
```

```rust
        self.bad.contains(bit_of(u64::from(section.first + group)), Relaxed)
```

Why: one atomic bit set in the tree (`blaze_ids::bits`), its word arithmetic and its
thread test (`atomic_bits_set_from_many_threads_all_land`) shared with the store's flags.

Status: open

> taken by requests (frontend-image is DONE): child `requests-bits` is applying it (pass 17).

> applied: by `requests-bits` (pass 17), hunk by hunk as written; the tree matched every before
> text. `crates/blaze-image/src/image.rs`: imports to `ops::Range, sync::atomic::Ordering::Relaxed`
> and `bits::AtomicBits`; `checked`/`bad` are `AtomicBits`; `Touched`'s test fields name
> `std::sync::atomic::AtomicU64` in full; `words` and `bit` deleted for `bit_of`; `is_checked`,
> `ensure`, `check`, `table_entry` and `is_bad` on `contains`/`insert`. One difference: the
> `Image { .. }` literal in `open` is over 100 columns on one line, so it is broken one field a
> line as rustfmt does. `rg` finds no `words(`, `bit(` or bare `AtomicU64` left in blaze-image.
> `hot_lint` (its `lint_file`, since `main` rejects a `crates` list): 0 findings, as before.
> Not part of this request, found in the same file and already in HEAD: `impl Image` has two
> methods named `check` (`pub fn check(&self, &Section, Range<u32>)` and the cold
> `fn check(&self, &Section, u32)` that `ensure` calls), which is E0592. The fix is to rename the
> private one (to `check_group`, say) and its one caller in `ensure`.
