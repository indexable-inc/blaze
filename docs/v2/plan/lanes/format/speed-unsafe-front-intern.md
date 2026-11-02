---
status: plan
summary: intern, span, tt, mbe, syntax-bridge and const-ast under unsafe.md: intern's unsafe gathered into two modules with safe APIs under a deny root, span's hygiene made safe, what each remaining unsafe buys, the untrusted-bytes checks of a restored table, the reference and model tests, and the fuzz target filed for the table section.
date: 2026-10-29
---
# speed-unsafe-front-intern: the interner and token crates under unsafe.md

The rules are [unsafe.md](../../design/unsafe.md). Under each rule:
- one operation per block;
- a `// SAFETY:` comment above each block, naming the invariant, where it is upheld and what
  breaks without it;
- `# Safety` on every `unsafe fn`;
- a module doc saying what the safe version would cost.

Every crate here takes the workspace lints (`[lints] workspace = true`), which deny
`unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. None has a `tests/`, `benches/` or `examples/` root:
their tests are in-crate modules under the root's attribute.

## intern

Root: `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` on `mod space` and `mod typed` only.
Test and model modules that write raw `u64`s through the store keep their own `allow`, each
with its reason.

- `space`: the 64 GiB reserved region.
  - A claim is one `fetch_add` and returns an offset. `at(offset)` takes the address from the
    region, so every pointer keeps the region's provenance.
  - `zeroed` reads fresh pages as a probe table of `Zeroed` types (atomics, `Group`). The safe
    version writes every group once more on each doubling and faults its pages in early.
  - `place` is `Box::leak` into the region.
  - `Published<T>` hands out a `&'static T` from an atomic pointer. The safe version is a lock
    or a count per lookup.
  - `Slots<T>` reads committed region words as atomic pointers: the ID column. The safe version
    is a `Vec` behind a lock. The model build keeps it in doubling `OnceLock` levels.
- `typed`: typed stores over `arena`'s untyped one.
  - `Values<T>` and `Ptr<T>` back `Interned`.
  - `Slices<H, S>` and `SlicePtr` back `InternedSlice`.
  - `Records` and `Record` hold a table value made in this process: a 16-byte header and its
    bytes, plus the ID column. A `Record` is a raw `NonNull<Head>`, because the bytes past the
    header are read through it and a `&Head`'s provenance would not cover them.
  - What the unsafe buys: a handle is one pointer read with no check, and a value is written
    straight into its slot.
- `arena` is safe code now. `Bump` keeps an offset, `Table::new` uses `space::zeroed`, and
  shards publish through `Published`.
- `symbol::text` holds the one `str::from_utf8_unchecked`. The table checked UTF-8 on restore,
  and a value interned here came from a `&str`.
- `impl_internable!` and `impl_slice_internable!` expand no unsafe into other crates.
- Untrusted bytes are checked in `table::Table::validate`, run on every restore before any
  value is read in place:
  - blaze-image's views check bounds and alignment;
  - the root: first ID, end, count;
  - the starts: in order, and each value's end within the values. This end check was missing,
    so a damaged start could panic a slice before the order check ran. It is fixed and tested;
  - UTF-8 for a text table;
  - the probe groups and the free list.
  - Each failure is `RestoreError::Invalid { section, what, at }`, naming the byte offset in
    the section.
- Tests:
  - `space`: claims from threads are disjoint, aligned and zero; offsets are addresses; slots
    are committed lazily; claims allocate nothing.
  - `table/tests.rs` `interning_agrees_with_a_map_model`: against an `FxHashMap<Vec<u8>, u32>`
    over three seeds, in the overlay and after a write and a restore. Also a start past the
    values, and a non-UTF-8 value, each refused where it is.
  - `intern.rs`: against a map from value to its first handle.
  - `table/model.rs` (`blaze_check`): racing interns get one ID each; a freed ID is taken once.
    The arena, intern and intern_slice models already existed.
  - Not modelled:
    - the `space` cursor, which is process-wide, so the checker can't reset it per execution.
      The threaded test covers it;
    - `Bump`, which runs under the shard lock and is covered by the arena models;
    - `Live`, which is single-threaded `&mut`.
- Fuzz: `intern_table`, filed in `v2/requests/safety.md` ("From speed-unsafe-front-intern").
  Its hook is `intern::fuzz` (feature `fuzz`), a third unsafe module. `Table::restore` needs
  a `&'static Image`, so the hook frees each input's image after the table drops. The safe
  version leaks every input's image. The hook also checks that a restored table, written again,
  restores to the same values.

## span

Root: `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` on `lines`'s `mod simd` only.

- `hygiene` is safe now, and its `allow` is gone.
  - `SyntaxContext::from_u32` is a safe `fn` (queue 00792). A wrong value panics or names
    another context; it was never UB.
  - `SyntaxContext::id` builds its `salsa::Id` with the checked `Id::from_bits(index + 1)`,
    after the bound assert. This is the same id `from_index` gave (generation 0).
  - `#[salsa::interned(unsafe(no_lifetime))]` expands its `unsafe impl`s from another crate's
    macro, which `unsafe_code` does not report. `hir-expand` forbids `unsafe_code` and uses the
    same option.
- `lines/simd`: AVX-512BW, AVX2, SSE2 or NEON, chosen at run time (`Kernel::available`,
  `Kernel::best`), with a scalar path. `every_kernel_gives_the_scalar_bits` checks every kernel
  the CPU runs against scalar, at four misalignments.

## tt

Root: `#![deny(unsafe_code)]`. Only `storage/owned.rs` says `#![allow(unsafe_code)]`.

- `owned`: a tree's block in any allocator, read through one non-generic `Tree`, so a view is
  one pointer. Left as unsafe:
  - `Send` and `Sync`;
  - the block's `as_ref` and `as_mut`;
  - `Drop`'s `ManuallyDrop::take` and `Box::from_raw_in`.
  - `NonNull::new_unchecked` became `NonNull::new(..).expect(..)`: once a tree.
- `columns`: no unsafe, and no allow. Its `from_u32` block is gone. zerocopy's derives emit
  `unsafe impl`s, but `unsafe_code` has no `report_in_external_macro`, so unsafe from another
  crate's macro is never reported.
- Reference test: `storage/tests.rs` `check`, against `Vec<TokenTree>` and the old varint
  encoding. It covers flat, iterator, cursor, display, rebuild, framed trees in a task frame,
  and the wire with every prefix and bit flip.

## mbe, syntax-bridge, const-ast

Each root is `#![forbid(unsafe_code)]`, and none has unsafe: the only `unsafe` hits are keyword
lists.
