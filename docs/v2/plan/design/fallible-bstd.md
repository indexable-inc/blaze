---
status: plan
summary: In bstd and blaze-mem every operation that can fail returns a typed error and there is no panicking, aborting or `try_` twin. Collections in an allocator grow fallibly and name the arena; sizes and offsets are checked types; formatting has one core, into a growing string or a lent buffer; non-test code holds no unwrap, expect, panic or panicking index, held by lints. What is built and what is left is in the last section.
date: 2026-11-12
---
# Fallible bstd: one form, and it returns the error

Decision D29 (docs/v2/plan/decisions.json). Scope: `crates/blaze-std` (bstd) and
`crates/blaze-mem`, non-test code.

## The rule

1. **One form.** An operation that can fail returns `Result` (or `Option` where absence is the
   only failure). There is no second form that panics, aborts or unwinds, and no `try_` prefix:
   `push` is the fallible push.
2. **Every failure, not only memory.** Out of memory, an index or range out of bounds, arithmetic
   on sizes that does not fit, bytes that do not decode, a limit or quota passed, a formatting
   trait that fails. Each error names the thing and the rule (`errors-name-the-thing`).
3. **No panic path in non-test code.** No `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`,
   `unimplemented!`, and no panicking index or slice (`a[i]`, `a[i..j]`). A case becomes explicit
   control flow that returns a typed error (`let Some(x) = .. else { return Err(..) }`, `?`), or
   a type that cannot hold the impossible case (a non-empty type, an index checked once where it
   is made, the value returned from where its invariant was established). Tests and benches are
   exempt.
4. **No fallback, no retry.** A growth that fails leaves the collection as it was and returns.

## Why

`alloc`'s `Vec::push` over an allocator that says no calls `handle_alloc_error`, which aborts the
process (the tree sets no hook). A bstd arena has a limit, so this was reachable by any caller
that grew a `Vec<T, &Frame>` past it, against std.md's "running out is an outcome, not a crash".
Shown, not assumed: `the_oracle_catches_a_push_routed_to_allocs_infallible_one`
(crates/blaze-mem/src/vec/tests.rs) runs the same pushes in a child process through `alloc`'s
push and through blaze-mem's; the first dies of SIGABRT with "memory allocation of 8192 bytes
failed", the second returns `Err` and goes on.

The losing alternative: keep infallible forms and turn the failure into the task's failure
through an unwind (std.md's earlier text). Rejected: the failure is then invisible in every
signature, and an unwind out of `alloc` is not something `alloc` promises to survive.

## The design

| piece | home | shape |
|---|---|---|
| `OutOfMemory { arena, asked }` | blaze-mem `error.rs` | what a collection's growth returns; `Copy`, comparable |
| `Named` | blaze-mem `error.rs` | an allocator says whose memory it is: `Frame` (its `Frame::named` name, else its tag), `ArenaLocal`, `Kept`, `Global` |
| `Vec<T, A>` | blaze-mem `vec.rs` | `alloc`'s `Vec` inside, grown only by `try_reserve`, written within capacity; `push`, `reserve`, `extend_from_slice`, `extend`, `insert`, `resize`, `with_capacity_in` return `Result` |
| `String<A>`, `OsString<A>`, `PathBuf<A>` | blaze-mem `text.rs` | over that `Vec`; `push`, `push_str`, `join`, `write_fmt` return `Result`; `truncate` inside a character is `InsideChar` |
| `format_in!(alloc, ..)` | blaze-mem `text.rs` | `Result<String<A>, FormatError<OutOfMemory>>` |
| `format_into!(&mut buf, ..)` | blaze-mem `text.rs` | `Result<&mut str, FormatError<TooSmall>>`, allocates nothing; `TooSmall { needed, room }` says what to lend |
| one formatting core | blaze-mem `text.rs` | `write_to<T: Take>`: a `Take` is where text goes (a string, a lent buffer); both macros are it |
| `ByteLen`, `Offset` | blaze-mem `size.rs` | `u64` newtypes with checked `plus`, `minus`, `times`, `after`, `since` returning `Overflow { what, left, right }`; no `+`, so the units do not mix |
| `Map<K, V, A>` | bstd `map` | blaze-hash's map inside; `insert` reserves fallibly first |
| `Stopped` | bstd | `From<OutOfMemory>` and `From<MemError>` give `Stopped::OutOfMemory { arena, asked }` |

blaze-mem's own allocators (`Frame::alloc`, `RegionVec::push`, `AppendVec::append`,
`Slab::insert`, `Cache::alloc`, `Cache::share`) keep `MemError`, which names the tag and the
sizes; their panicking twins go and the `try_` names lose the prefix, with every caller in the
workspace ported in the same change.

A formatting trait that returns an error though its writer took every byte is
`FormatError::Trait` (std's `format!` panics there).

`Clone` and `Extend` are not implemented for these collections: both are infallible traits that
allocate. `clone_in_place` and `extend` are their fallible forms.

## Prior art

- **Zig**: every allocating call takes an `Allocator` and returns `error{OutOfMemory}!T`
  (`ArrayList.append`); `appendAssumeCapacity` is the write within reserved room. There is no
  infallible form. This is the model.
- **Rust for Linux** (`rust/kernel/alloc/kvec.rs`): `KVec::push(value, flags) -> Result<(),
  AllocError>`, `reserve`, `extend_from_slice`, all fallible; the kernel builds `alloc` with
  `no_global_oom_handling`, which removes `Vec::push` itself. blaze cannot set that cfg for
  `alloc` while cargo builds it, so the ban is a lint here.
- **Rust's `alloc`**: `try_reserve` plus a write within capacity (`push_within_capacity`) is the
  supported fallible path, and is what `Vec` here is built on.

## The floor

A growable vector's push must compare its length with its capacity: one compare and branch, one
write, one store of the length. `Vec::push` here is that: the growth is `#[cold]` and out of
line, and the `Err` is made only there, so once inlined the caller's `?` folds into the same
branch. Design cost against the floor: 1.0 on the path with room (not measured on generated
code yet; the claim is from the source). The error path costs what the failed `try_reserve`
costs. `OutOfMemory` is two words and `Result<(), OutOfMemory>` has a niche, so `Ok(())` is one
register. `format_into!` copies what fits and counts the rest: one pass, no allocation (held by
a test with the counting allocator).

What no design reaches: a failure inside `alloc` or a dependency that is infallible by its own
signature (`hashbrown`'s `insert` after a successful `try_reserve` cannot fail; `Arc::new` can,
so bstd uses `Arc::try_new`).

## Lints

Per crate, non-test code, `deny`, no `allow` or `expect` attribute:
`clippy::unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, `unimplemented`,
`indexing_slicing`, plus `disallowed_methods` and `disallowed_macros` for `alloc`'s infallible
growth (`Vec::push`, `extend*`, `insert`, `reserve`, `resize`, `with_capacity*`, `String::push*`,
`Box::new*`, `Arc::new`, `format!`, `vec!`, `to_vec`, `to_owned`, `to_string`) in
`crates/blaze-mem/clippy.toml` and `crates/blaze-std/clippy.toml`. `arithmetic_side_effects` is
not turned on: sizes that come from a caller go through `ByteLen` and `Offset` or `checked_*`;
address arithmetic inside the allocators stays (300 sites, each within a block the allocator
owns).

## How it is proven

- blaze-mem (`cargo test -p blaze-mem`): a Hegel property test draws growth operations (push,
  extend, reserve, insert, resize, `push_str`, `write!`, `format_in!`, a path push) against a
  frame of a drawn limit with global-heap twins as the model; the oracle is that every growth
  returns, an `Err` names the arena and leaves the collection equal to the model, the frame
  never passes its limit, and another frame still grows. Its control plants `alloc`'s push and
  must see the process abort.
- bstd: the same oracle in the simulation (`Io::sim`), through `Bytes`, `Vec`, `String`,
  `format_in!`, `Map::insert` and a directory listing, with another task that goes on.
- Sizes: planted overflows return `Overflow`. `format_into!`: a small buffer returns the bytes
  needed.

## What is built, and what is left

Kept current as the work lands; the lane's report has the detail.
