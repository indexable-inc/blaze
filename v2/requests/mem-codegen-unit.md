# mem-codegen-unit: moving `hir-ty-codegen` and `blaze-codegen` onto `blaze-mem`

From `mem-codegen-unit` (lane `mem`, under `mem-codegen`), for the owners of
`crates/hir-ty-codegen/` and `crates/blaze-codegen/`: lane `codegen` (`codegen-next1`) and the
speed agents that name them (`speed-hir-store-next1`, `speed-hir-store-paths-consumers`). I edit
none of these files; every change below is for the owner to apply.

Line numbers are those of the worktree on 2026-10-29, uncommitted edits included. Both crates are
being edited right now (`Template` and `CompiledFunction` gained `coverage` today), so each change
also names its function. Apply a change by its function and its before text, not by line alone.

What every change keeps:

- **Output.** Every byte of output and every error is the same: the same functions, templates,
  shapes, side tables and store keys, the same messages, in the same order.
- **Iteration order.** A hash map moves into a frame only where nothing iterates it. One map that
  is iterated, `registers`, becomes a dense map whose order is index order, which is the order its
  keys were sorted into before.
- **Frame lifetimes.** A `Vec<T, &Frame>` or frame slice never outlives its `frame::task`. What a
  unit keeps (its IR, its `Function`, its `CompiledFunction`, its template, its side bytes) stays
  on the global heap, allocated once at its exact size.

## Contents

0. Prerequisites for other owners (root `Cargo.toml`, `blaze-mem` on wasm32, the hot lint,
   `blaze-cargo`)
1. `crates/hir-ty-codegen`
   - 1.1 `Cargo.toml` and `mod.rs` header
   - 1.2 new `frame_map.rs`
   - 1.3 `lent.rs`
   - 1.4 `mod.rs`: `build()` and `FunctionCx` on a frame
   - 1.5 per-worker relocation and line buffers (`fragment.rs`, `debuginfo.rs`, `spare.rs`,
     `mod.rs`)
   - 1.6 `v2/places.rs`
   - 1.7 `v2/convert.rs`
   - 1.8 `v2/backend.rs`
   - 1.9 `v2/shape.rs`
   - 1.10 `v2/side.rs`
   - 1.11 `v2/release.rs`, `v2/statics.rs`
   - 1.12 Cranelift's context
   - 1.13 tests
   - 1.14 what stays, and why
2. `crates/blaze-codegen`
   - 2.1 `Cargo.toml` and `lib.rs`
   - 2.2 `backend.rs`
   - 2.3 `template.rs`
   - 2.4 `blob.rs`
   - 2.5 `driver.rs`
   - 2.6 tests
   - 2.7 `// alloc:` notes for the hot lint
3. Structs that gain a lifetime or an allocator parameter, and their callers
4. Boundary with `blaze-engine` (agreed with `mem-codegen-engine`)
   - 4.1 where the frames open
   - 4.2 `SymbolResolver`
   - 4.3 its callers in `hir-ty-codegen`
   - 4.4 `spare::with_copies` and its test
   - 4.5 the other implementors (`blaze-wasm`)

---

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

## 0. Prerequisites for other owners

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 0.1 Root `Cargo.toml` (coordinator): hashbrown's `nightly` feature

hashbrown only takes a `core::alloc::Allocator` (what `&blaze_mem::Frame` is) with its `nightly`
feature; without it, its allocator parameter is its own `allocator-api2` trait, which `Frame` does
not implement. Change line 168:

```toml
hashbrown = { version = "0.17.0", features = ["inline-more"], default-features = false }
```

to:

```toml
hashbrown = { version = "0.17.0", features = ["inline-more", "nightly"], default-features = false }
```

`hir-ty-codegen` already depends on `hashbrown.workspace = true`. `blaze-codegen` needs no hash map
in a frame.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 0.2 `blaze-mem` on wasm32 (lane `mem`, `crates/blaze-mem`)

`hir-ty-codegen` is also compiled to wasm32: `crates/blaze-wasm/Cargo.toml:28` builds it with its
`wasm` feature. On that target `blaze_mem::os::reserve` (`crates/blaze-mem/src/os.rs:111-117`)
returns `Unsupported`, "blaze-mem reserves address space on Linux, macOS and Windows only". So after
1.4, the first `frame::task` in `build()` stops the wasm32 compiler with that error. That is the
right behaviour (no fallback), but it breaks `blaze-wasm` until `blaze-mem` can make a frame on
wasm32. A wasm32 linear memory has no address space to reserve, and
`frame::FRAME_RESERVE = 4 << 30` is all of a 32-bit space. What `blaze-mem` needs on
`not(any(unix, windows))`:

- `reserve` takes the bytes from the linear memory (`memory.grow`, or `std::alloc::alloc` of the
  whole reserve, once per frame);
- `commit` and `decommit` do nothing;
- `FRAME_RESERVE` is a few MiB on `target_pointer_width = "32"`.

Every frame use below is on the native path. Of what `blaze-wasm` reaches, only `build()` (1.4) has
a frame; v2 refuses wasm units (`v2/backend.rs` `check_spec`). I am telling my parent
(`mem-codegen`) of this in my report.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

> done by mem (2026-10-29), as proposed. In `crates/blaze-mem/src/os.rs` on
> `not(any(unix, windows))`, `reserve` takes the whole range once, zeroed, from the global
> allocator, and `release` gives it back. `commit` does nothing, and `decommit` zeroes the range
> so it reads zeros when committed again, as on Linux. On `not(target_pointer_width = "64")`,
> `frame::FRAME_RESERVE` is 16 MB and `chunks::SPACE_BYTES` is 256 MB. Before this change,
> blaze-mem did not even compile for wasm32: `4 << 30` and `256 << 30` overflow a 32-bit
> `usize`. A caller elsewhere that reserves a 64-bit-sized `RegionVec` on wasm32 now stops with
> `MemError` naming its tag (no fallback), which is the caller's to size per target.

### 0.3 The hot lint never reads `hir-ty-codegen` (coordinator, `tools/v2/hot_lint.py`)

- **It never reads `hir-ty-codegen`.** It reads `crates/<crate>/src` only (`hot_lint.py:89`).
  `hir-ty-codegen`'s library root is `crates/hir-ty-codegen/mod.rs` with no `src/`, so the lint
  reports nothing for it.
- **Its `"crates"` input can't be given.** `{"crates": [...]}` fails with
  `TypeError: typed_input: crates has a type it does not read: list[str]`.

Both gaps are the lint's. The `// alloc:` notes in this file are written for the lines the lint
would flag once it reads `hir-ty-codegen`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 0.4 `crates/blaze-cargo/src/serve/unit/codegen.rs:240` (lane `cargo-daemon-units`, DONE)

The one other implementor of `blaze_codegen::driver::Lookup`. It changes with 2.5, in the same
change, or the workspace stops building:

```rust
impl blaze_codegen::driver::Lookup for Reading<'_> {
    fn get(&self, key: &blaze_cas::Key) -> Result<Option<Vec<u8>>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes.to_vec())),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}
```

becomes:

```rust
impl blaze_codegen::driver::Lookup for Reading<'_> {
    fn read(&self, key: &blaze_cas::Key) -> Result<Option<&[u8]>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}
```

`self.store.get(self.guard, key)` gives an `Entry<'a>`, where `'a` is `Reading`'s lifetime
(`store` and `guard` are both `&'a`, `crates/blaze-cas/src/lib.rs:403`). So its bytes outlive
the `&self` borrow `read` returns them under.

`Entry::bytes` is `&'g [u8]`, verified by the store (`crates/blaze-cas/src/lib.rs:219`). The
store's hit was copied into a `Vec` on every read; now it is read in place.

---

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

## 1. `crates/hir-ty-codegen`

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.1 `Cargo.toml` and the crate root

> applied by codegen-v1 (2026-10-29): the `blaze-mem` dependency, the `blaze-alloc-count` dev-dependency, `#![feature(allocator_api)]`, `mod frame_map;`, the `Frame`/`frame_map` uses and the test counting allocator, as written.

`crates/hir-ty-codegen/Cargo.toml`, `[dependencies]`, beside the other `blaze-*` lines:

```toml
blaze-mem.workspace = true
```

`[dev-dependencies]`:

```toml
blaze-alloc-count.workspace = true
```

`crates/hir-ty-codegen/mod.rs`. After the `//!` header (lines 1-4) and before
`extern crate ra_ap_rustc_ast_ir` (line 6):

```rust
#![feature(allocator_api)]
```

The crate has no `#![...]` yet; inner attributes must come before the `extern crate` items.

Beside `mod fragment;` (line 119):

```rust
mod frame_map;
```

With the crate's other `use`s:

```rust
use blaze_mem::Frame;
use frame_map::{FrameHashMap, FrameHashSet, FrameMap};
```

Before `#[cfg(test)] mod tests` (line 19982):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

`mod.rs` has a `type Tag = Option<(usize, types::Type, u128)>` of its own (line 8402), so every
use of `blaze_mem`'s tag below is spelled `blaze_mem::Tag::Codegen`, never imported as `Tag`. The
crate's own `pub mod counted` has a type named `Counting` that is no allocator; the allocator is
always spelled `blaze_alloc_count::Counting`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.2 New file `crates/hir-ty-codegen/frame_map.rs`

> applied by codegen-v1 (2026-10-29): `frame_map.rs` as written, with both tests.

```rust
//! The maps and sets one function's build keeps while it runs, in its frame
//! ([`blaze_mem::frame::task`]): dropped with the frame when the build returns, so a warm build
//! asks the global allocator for none of them.

use std::{marker::PhantomData, ops::Index};

use blaze_mem::Frame;
use la_arena::{Idx, RawIdx};
use rustc_hash::FxBuildHasher;

/// An `FxHashMap` in a frame. Only for a map nothing iterates where the order shows: its order
/// is the table's.
pub(crate) type FrameHashMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;

/// An `FxHashSet` in a frame, as [`FrameHashMap`].
pub(crate) type FrameHashSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;

pub(crate) fn hash_map<'f, K, V>(frame: &'f Frame) -> FrameHashMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

pub(crate) fn hash_map_with_capacity<'f, K, V>(capacity: usize, frame: &'f Frame) -> FrameHashMap<'f, K, V> {
    hashbrown::HashMap::with_capacity_and_hasher_in(capacity, FxBuildHasher, frame)
}

pub(crate) fn hash_set<'f, T>(frame: &'f Frame) -> FrameHashSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}

/// A value for some of an arena's indices, by index, in a frame: `la_arena::ArenaMap`'s layout
/// (one `Option<V>` a slot, up to the largest index given) and its answers. [`FrameMap::keys`]
/// is in index order.
pub(crate) struct FrameMap<'f, K, V> {
    slots: Vec<Option<V>, &'f Frame>,
    len: usize,
    key: PhantomData<fn() -> K>,
}

impl<'f, T, V> FrameMap<'f, Idx<T>, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        Self::with_capacity_in(0, frame)
    }

    pub(crate) fn with_capacity_in(capacity: usize, frame: &'f Frame) -> Self {
        FrameMap { slots: Vec::with_capacity_in(capacity, frame), len: 0, key: PhantomData }
    }

    fn slot(idx: Idx<T>) -> usize {
        idx.into_raw().into_u32() as usize
    }

    /// Gives `idx` `value`, returning the value it had.
    pub(crate) fn insert(&mut self, idx: Idx<T>, value: V) -> Option<V> {
        let at = Self::slot(idx);
        if at >= self.slots.len() {
            self.slots.resize_with(at + 1, || None);
        }
        let old = self.slots[at].replace(value);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub(crate) fn get(&self, idx: Idx<T>) -> Option<&V> {
        self.slots.get(Self::slot(idx)).and_then(Option::as_ref)
    }

    pub(crate) fn contains_idx(&self, idx: Idx<T>) -> bool {
        self.get(idx).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The indices with a value, in index order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = Idx<T>> + '_ {
        let given = self.slots.iter().enumerate().filter(|(_, it)| it.is_some());
        given.map(|(at, _)| Idx::from_raw(RawIdx::from(at as u32)))
    }
}

impl<T, V> Index<Idx<T>> for FrameMap<'_, Idx<T>, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, idx: Idx<T>) -> &V {
        match self.get(idx) {
            Some(it) => it,
            None => panic!("no value for index {}", Self::slot(idx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};
    use la_arena::{Arena, ArenaMap};

    use super::*;

    /// Each index's value is the last one given, and `keys` is in index order whatever order
    /// they came in: what `ArenaMap` answers.
    #[test]
    fn a_frame_map_answers_as_an_arena_map() {
        let mut arena = Arena::new();
        let ids: Vec<Idx<u32>> = (0..64).map(|it| arena.alloc(it)).collect();
        frame::task(Tag::Codegen, |frame| {
            let mut map = FrameMap::new_in(frame);
            let mut reference = ArenaMap::default();
            for (step, &id) in ids.iter().rev().step_by(3).enumerate() {
                assert_eq!(map.insert(id, step), reference.insert(id, step));
            }
            assert_eq!(map.insert(ids[3], 99), reference.insert(ids[3], 99));
            for &id in &ids {
                assert_eq!(map.get(id), reference.get(id));
                assert_eq!(map.contains_idx(id), reference.contains_idx(id));
            }
            let keys: Vec<Idx<u32>> = map.keys().collect();
            let expected: Vec<Idx<u32>> = reference.iter().map(|(id, _)| id).collect();
            assert_eq!(keys, expected);
            assert_eq!(map.len(), expected.len());
            assert_eq!(map[ids[3]], 99);
        });
    }

    /// A warm unit's maps and sets take nothing of the global allocator.
    #[test]
    fn a_warm_frame_map_allocates_nothing() {
        let mut arena = Arena::new();
        let ids: Vec<Idx<u32>> = (0..256).map(|it| arena.alloc(it)).collect();
        let fill = |frame: &Frame| {
            let mut map = FrameMap::with_capacity_in(ids.len(), frame);
            let mut set = hash_set(frame);
            let mut by = hash_map(frame);
            for (at, &id) in ids.iter().enumerate() {
                map.insert(id, at);
                set.insert(id);
                by.insert(id, at);
            }
            (map.len(), map.keys().count(), set.len(), by.len())
        };
        let cold = frame::task(Tag::Codegen, &fill);
        let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's frame maps and sets", || {
            frame::task(Tag::Codegen, &fill)
        });
        assert_eq!(cold, warm);
    }
}
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.3 `crates/hir-ty-codegen/lent.rs`: an allocator for the list

> applied by codegen-v1 (2026-10-29): as written, plus `Lent::from_room`/`into_room` (an empty list in a buffer given back, and the buffer back emptied; used by 1.4's `KeptLayouts`) and the test `a_list_in_given_back_room_starts_empty`.

`KeptLayouts` (1.4) keeps its two `Lent` lists of pointers in the build's frame. Each target stays
in its own `Arc` or `Box` allocation: a `Frame::alloc` value's destructor never runs, and a
`Layout` owns heap memory.

Line 7:

```rust
use std::{cell::RefCell, ops::Deref, sync::Arc};
```

becomes:

```rust
use std::{
    alloc::{Allocator, Global},
    cell::RefCell,
    ops::Deref,
    sync::Arc,
};
```

Lines 9-18:

```rust
/// ... value; nothing is taken out before the list is dropped.
pub(crate) struct Lent<P>(RefCell<Vec<P>>);

impl<P> Default for Lent<P> {
    fn default() -> Self {
        Lent(RefCell::new(Vec::new()))
    }
}
```

become:

```rust
/// ... value; nothing is taken out before the list is dropped. The list's own pointers are
/// `A`'s (a build's frame); what they point at is in allocations of its own.
pub(crate) struct Lent<P, A: Allocator = Global>(RefCell<Vec<P, A>>);

impl<P> Default for Lent<P> {
    fn default() -> Self {
        Lent(RefCell::new(Vec::new()))
    }
}

impl<P, A: Allocator> Lent<P, A> {
    /// An empty list whose pointers are kept in `alloc`.
    pub(crate) fn new_in(alloc: A) -> Self {
        Lent(RefCell::new(Vec::new_in(alloc)))
    }
}
```

Line 36, `impl<P: Stable> Lent<P> {`, becomes `impl<P: Stable, A: Allocator> Lent<P, A> {`. The body
of `alloc` is unchanged. Its `SAFETY` comment holds as it is: growth of the list moves the pointers
in `A`'s buffer, never their targets, and dropping the `Vec` drops every `P`. Add to it: "`A`
holds only the list's buffer of pointers; a frame is reset only through `&mut Frame`, which can't
be had while the list borrows it."

Test, added to `lent.rs`'s `mod tests`:

```rust
    /// A list kept in a frame lends as one on the heap does, and its pointers take nothing of
    /// the global allocator once the frame is warm.
    #[test]
    fn a_lent_value_in_a_frame_list_is_the_one_kept() {
        let boxes: Vec<Box<u64>> = (0..512).map(Box::new).collect();
        let lend = |frame: &blaze_mem::Frame| {
            let list: Lent<&u64, &blaze_mem::Frame> = Lent::new_in(frame);
            boxes.iter().map(|it| std::ptr::eq(list.alloc(&**it), &**it)).all(|same| same)
        };
        assert!(blaze_mem::frame::task(blaze_mem::Tag::Codegen, &lend));
        let warm = blaze_alloc_count::assert_at_most(0, "a warm frame list of pointers", || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, &lend)
        });
        assert!(warm);
    }
```

with this added to the module, after `unsafe impl<T> Stable for Box<T> {}`:

```rust
// SAFETY: a shared reference's target is not where the reference is, and outlives it.
unsafe impl<T> Stable for &T {}
```

That impl is sound for the same reason the `Arc` and `Box` ones are: moving a `&T` leaves its
target in place, alive for the reference's lifetime.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.4 `crates/hir-ty-codegen/mod.rs`: each function's build on a frame

> applied by codegen-v1 (2026-10-29): as written, except `KeptLayouts`: `&'cx KeptLayouts<'cx>` from the frame cannot borrow-check (it is invariant, and its frame `Vec` must be live at its own drop, so `kept` would still be borrowed when dropped), so `KeptLayouts` stays without a lifetime and takes its two lists from a per-thread shelf (`spare::layout_lists`/`keep_layout_lists`, `KeptLayouts::keep()` after the task), which allocates nothing warm; `Layouts::new(&kept, frame)` and the rest are as written, and the test at the end also checks `keep` drops every layout.

`build()` (5061-5136) is where every function is built, v1's, v2's and the shims. It opens one
`frame::task(Tag::Codegen, ..)`. `FunctionCx` and everything it keeps for the build live in that
frame. A build inside a build (a callee built in) gets a frame of its own (the task nests). Up to
8 nested frames per thread stay warm (`blaze_mem::frame::DEPTH`); `spare.rs` keeps 4 Cranelift
contexts the same way.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `GivenAway` (8269-8284)

Before:

```rust
/// The locals a call's operands move whole, whose memory the callee can be handed as it is.
struct GivenAway {
    /// The local the call's result is written to.
    result: LocalId,
    whole: FxHashSet<Value>,
    /// The address of every place the call's operands read.
    read: Vec<Value>,
}

impl GivenAway {
    /// Whether the memory at `addr` is a local moved whole that no other operand reads: a
    /// field at the start of a local has the local's address.
    fn gives(&self, addr: Value) -> bool {
        self.whole.contains(&addr) && self.read.iter().filter(|&&it| it == addr).count() == 1
    }
}
```

After. One set and one list for the whole build, emptied at each call. Before, each call made a
new `FxHashSet` and `Vec`.

```rust
/// The locals a call's operands move whole, whose memory the callee can be handed as it is:
/// one for the whole build, emptied at each call ([`GivenAway::start`]).
struct GivenAway<'f> {
    /// While a call is generated: the local its result is written to.
    result: Option<LocalId>,
    whole: FrameHashSet<'f, Value>,
    /// The address of every place the call's operands read.
    read: Vec<Value, &'f Frame>,
}

impl<'f> GivenAway<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        GivenAway { result: None, whole: frame_map::hash_set(frame), read: Vec::new_in(frame) }
    }

    /// Starts the call whose result goes to `result`, forgetting the last call's.
    fn start(&mut self, result: LocalId) {
        self.result = Some(result);
        self.whole.clear();
        self.read.clear();
    }

    /// Whether, while a call is generated, the memory at `addr` is a local moved whole that no
    /// other operand reads: a field at the start of a local has the local's address.
    fn gives(&self, addr: Value) -> bool {
        self.result.is_some()
            && self.whole.contains(&addr)
            && self.read.iter().filter(|&&it| it == addr).count() == 1
    }
}
```

The semantics are the same. Before, `given_away: None` meant no call was being generated; now
that is `result: None`. A call inside a call (made while its operands are generated) used to
replace the outer `Some` and leave `None` when it returned. Now it calls `start` and leaves
`result: None` when it returns. Either way, after the inner call the outer one has no locals
given away.

Its three users:

- 9754:

  ```rust
                if self.given_away.as_ref().is_some_and(|it| it.gives(addr)) {
  ```

  becomes:

  ```rust
                if self.given_away.gives(addr) {
  ```

- 11060-11070:

  ```rust
                if let Some(given) = &mut self.given_away {
                    given.read.push(addr);
                }
                if let OperandKind::Move(_) = operand.kind
                    && place.projection.as_slice().is_empty()
                    && self.glue_of(ty.ty) != DropGlue::None
                    && let Some(given) = &mut self.given_away
                    && given.result != place.local
                {
                    given.whole.insert(addr);
                }
  ```

  becomes:

  ```rust
                if self.given_away.result.is_some() {
                    self.given_away.read.push(addr);
                }
                if let OperandKind::Move(_) = operand.kind
                    && place.projection.as_slice().is_empty()
                    && self.glue_of(ty.ty) != DropGlue::None
                    && self.given_away.result.is_some_and(|it| it != place.local)
                {
                    self.given_away.whole.insert(addr);
                }
  ```

  `glue_of` still runs first, as before (it fills the `glue` memo).

- 13642-13649:

  ```rust
        self.given_away = Some(GivenAway {
            result: destination.local,
            whole: FxHashSet::default(),
            read: Vec::new(),
        });
        let done = self.call_with(func, args, destination, target, intrinsic);
        self.given_away = None;
        done
  ```

  becomes:

  ```rust
        self.given_away.start(destination.local);
        let done = self.call_with(func, args, destination, target, intrinsic);
        self.given_away.result = None;
        done
  ```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `FunctionCx` (8301-8371)

It already has the lifetime `'cx`; it gains a field and changes types:

```rust
struct FunctionCx<'cx, 'db> {
    cg: &'cx mut Recorder<'db>,
    /// The build's frame: what the tables below are kept in, reset when the build returns.
    frame: &'cx Frame,
    ...
    locals: FrameMap<'cx, LocalId, LocalSlot>,
    blocks: FrameMap<'cx, BasicBlockId, ir::Block>,
    layouts: Layouts<'cx, 'db>,
    core_panics: Vec<(&'static str, Option<FunctionId>), &'cx Frame>,
    glue: std::cell::RefCell<FrameHashMap<'cx, Ty<'db>, DropGlue>>,
    ...
    pads: Vec<(BasicBlockId, ir::Block), &'cx Frame>,
    ...
    argument_slots: Vec<(ir::StackSlot, (u32, u8)), &'cx Frame>,
    free_argument_slots: Vec<(ir::StackSlot, (u32, u8)), &'cx Frame>,
    /// While a call is generated (`result` is set): the local its result goes to, and where
    /// the locals moved whole into it are. Nothing reads those again, so the callee can be
    /// handed them.
    given_away: GivenAway<'cx>,
    ...
    constant_locals: Vec<LocalId, &'cx Frame>,
    registers: FrameMap<'cx, LocalId, Register>,
    ...
    held_at: Vec<(LocalId, u32), &'cx Frame>,
}
```

The fields not shown are unchanged. `use la_arena::ArenaMap;` (line 48) has no other use in
`mod.rs` after this, so it goes.

What their users change:

- **`locals` and `blocks`.** `insert`, `get`, `contains_idx` and `[idx]` have `ArenaMap`'s
  signatures, so their users are unchanged.
- **`glue`, `layouts.of` and `layouts.variants`.** Hashbrown's `get`, `insert` and
  `entry(..).or_insert(..)` are std's, so 8732, 8736, 8741, 8759, 12741 and 12745 are unchanged.
- **`free_argument_slots.append(&mut self.argument_slots)` (15366).** Both are
  `Vec<_, &'cx Frame>`, so it is unchanged.
- **`registers`.** Its users change; see below.
- **18729:**

  ```rust
        for (target, pad) in std::mem::take(&mut self.pads) {
  ```

  becomes:

  ```rust
        for (target, pad) in std::mem::replace(&mut self.pads, Vec::new_in(self.frame)) {
  ```

  A `Vec<_, &Frame>` has no `Default`.

`registers` keys a `LocalId`, as `locals` does, and its arena is the body's locals, so a dense
map by index takes one `Option<Register>` per local, as `locals` does. Its users:

`mod.rs` moved by 16 lines while I wrote this, so these line numbers are from a second
`rg -n 'self\.registers\.' mod.rs`; rerun it before applying.

| line  | before                                                                          | after                                                                       |
| ----- | ------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| 10393 | `!self.registers.contains_key(&temp) && !self.registers.contains_key(&from)`    | `!self.registers.contains_idx(temp) && !self.registers.contains_idx(from)` |
| 10403 | `\|\| self.registers.contains_key(&local)`                                      | `\|\| self.registers.contains_idx(local)`                                   |
| 10815 | `self.registers.get(&local)`                                                    | `self.registers.get(local)`                                                 |
| 10834 | `self.registers.get(&place.local).copied()`                                     | `self.registers.get(place.local).copied()`                                  |
| 10841 | `self.registers.get(&place.local).copied()`                                     | `self.registers.get(place.local).copied()`                                  |
| 10920 | `match self.registers.get(local) {` (`local: &LocalId`)                         | `match self.registers.get(*local) {`                                        |
| 13322 | `self.registers.keys().copied().collect()` and the radix sort after it          | see `note_shareable` below                                                  |

`insert` (10389) and `is_empty` (10831) keep their signatures. The other `mod.rs` line numbers in
1.4 are from the first read, so expect the same shift of about 16 lines after line 10000.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Layouts` and `KeptLayouts` (8378-8409)

Before:

```rust
struct Layouts<'cx, 'db> {
    kept: &'cx KeptLayouts,
    of: std::cell::RefCell<FxHashMap<Ty<'db>, &'cx Layout>>,
    ...
    variants:
        std::cell::RefCell<FxHashMap<(VariantId, GenericArgs<'db>), Option<VariantShape<'cx>>>>,
}

/// What [`Layouts`] lends from, owned by `build` for the whole of it and freed after.
struct KeptLayouts {
    asked: Lent<Arc<Layout>>,
    variants: Lent<Box<Layout>>,
}

impl KeptLayouts {
    fn new() -> Self {
        Self { asked: Lent::default(), variants: Lent::default() }
    }
}
...
impl<'cx> Layouts<'cx, '_> {
    fn new(kept: &'cx KeptLayouts) -> Self {
        Self { kept, of: Default::default(), variants: Default::default() }
    }
}
```

After:

```rust
struct Layouts<'cx, 'db> {
    kept: &'cx KeptLayouts<'cx>,
    of: std::cell::RefCell<FrameHashMap<'cx, Ty<'db>, &'cx Layout>>,
    ...
    variants: std::cell::RefCell<
        FrameHashMap<'cx, (VariantId, GenericArgs<'db>), Option<VariantShape<'cx>>>,
    >,
}

/// What [`Layouts`] lends from, owned by `build` for the whole of it and freed after. Its lists
/// of pointers are in the build's frame; each layout stays salsa's (`Arc`) or the variant's own
/// copy (`Box`), freed when its list drops at the end of the build.
struct KeptLayouts<'f> {
    asked: Lent<Arc<Layout>, &'f Frame>,
    variants: Lent<Box<Layout>, &'f Frame>,
}

impl<'f> KeptLayouts<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        Self { asked: Lent::new_in(frame), variants: Lent::new_in(frame) }
    }
}
...
impl<'cx> Layouts<'cx, '_> {
    fn new(kept: &'cx KeptLayouts<'cx>, frame: &'cx Frame) -> Self {
        let of = std::cell::RefCell::new(frame_map::hash_map(frame));
        Self { kept, of, variants: std::cell::RefCell::new(frame_map::hash_map(frame)) }
    }
}
```

`KeptLayouts<'f>` is invariant in `'f` (a `RefCell` holds it). So `build()` makes it inside the
task's closure: there the `&Frame` shortens to a lifetime local to the closure, which both
`&kept` and `FunctionCx<'cx, ..>` take as their `'cx`. The two `let kept: &'cx KeptLayouts =
self.layouts.kept;` (8757, 12788) are unchanged: the elided lifetime is inferred.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `build()` (5061-5136)

Before (5067-5136, abridged where unchanged):

```rust
) -> Result<cranelift_codegen::Context> {
    let interner = DbInterner::new_with(cg.db, env.krate);
    let kept = KeptLayouts::new();
    let mut ctx = spare::context();
    ...
    let ub_checks = match body { ... };
    let mut fx = FunctionCx {
        cg,
        body,
        env,
        infcx: interner.infer_ctxt().build(TypingMode::PostAnalysis),
        interner,
        builder: FunctionBuilder::new(&mut ctx.func, &mut builder_ctx),
        locals: ArenaMap::with_capacity(body.map_or(0, |it| it.locals.len())),
        blocks: ArenaMap::with_capacity(body.map_or(0, |it| it.basic_blocks.len())),
        layouts: Layouts::new(&kept),
        core_panics: Vec::new(),
        glue: Default::default(),
        inner_pad: None,
        unwind_to: None,
        pads: Vec::new(),
        exception: None,
        argument_slots: Vec::new(),
        free_argument_slots: Vec::new(),
        given_away: None,
        ...
        constant_locals: Vec::new(),
        registers: FxHashMap::default(),
        args: None,
        suspended: None,
        held_at: Vec::new(),
    };
    fill(&mut fx)?;
    fx.note_variables();
    fx.builder.seal_all_blocks();
    let config = fx.cg.target_config();
    let guarded = fx.guarded;
    fx.builder.finalize(config);
    spare::keep_builder(builder_ctx);
    if guarded && let Some(entry) = ctx.func.layout.entry_block() {
```

After (the `let kept` at 5069 goes; 5070-5082 are unchanged):

```rust
) -> Result<cranelift_codegen::Context> {
    let interner = DbInterner::new_with(cg.db, env.krate);
    let mut ctx = spare::context();
    ...
    let ub_checks = match body { ... };
    // The build's tables (its locals, blocks, layouts, drop glue, registers and the rest of
    // `FunctionCx`) are in a frame of the worker's, reset when the build returns.
    let guarded = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| -> Result<bool> {
        let kept = KeptLayouts::new_in(frame);
        let mut fx = FunctionCx {
            cg,
            frame,
            body,
            env,
            infcx: interner.infer_ctxt().build(TypingMode::PostAnalysis),
            interner,
            builder: FunctionBuilder::new(&mut ctx.func, &mut builder_ctx),
            locals: FrameMap::with_capacity_in(body.map_or(0, |it| it.locals.len()), frame),
            blocks: FrameMap::with_capacity_in(body.map_or(0, |it| it.basic_blocks.len()), frame),
            layouts: Layouts::new(&kept, frame),
            core_panics: Vec::new_in(frame),
            glue: std::cell::RefCell::new(frame_map::hash_map(frame)),
            inner_pad: None,
            unwind_to: None,
            pads: Vec::new_in(frame),
            exception: None,
            argument_slots: Vec::new_in(frame),
            free_argument_slots: Vec::new_in(frame),
            given_away: GivenAway::new_in(frame),
            block: None,
            in_cleanup: false,
            aborts,
            debug_assertions: compiling,
            ub_checks,
            guarded: false,
            ret_mode: PassMode::Ignore,
            tracked,
            variadic: false,
            abi: ExternAbi::Rust,
            location: None,
            span: MirSpan::Unknown,
            constant_locals: Vec::new_in(frame),
            registers: FrameMap::new_in(frame),
            args: None,
            suspended: None,
            held_at: Vec::new_in(frame),
        };
        fill(&mut fx)?;
        fx.note_variables();
        fx.builder.seal_all_blocks();
        let config = fx.cg.target_config();
        let guarded = fx.guarded;
        fx.builder.finalize(config);
        Ok(guarded)
    })?;
    spare::keep_builder(builder_ctx);
    if guarded && let Some(entry) = ctx.func.layout.entry_block() {
```

The rest is unchanged.

- **Borrows.** `cg`, `interner` and `fill` move into the closure. `ctx.func` and `builder_ctx` are
  borrowed by it and free again when it returns, before `keep_builder` and the guarded fix-up.
- **Drop order.** `fx` is declared after `kept` and dropped before it.
- **A failed `fill`.** It returns the error from the closure, so `builder_ctx` is not kept, as
  before (its build failed half way).
- **Unwinding.** A panic unwinding out of `fill` resets the frame (`frame::task`'s guard); v1's
  `emit_with` `catch_unwind`s around Cranelift, not around this.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### Locals setup (10360-10383)

```rust
        let mut registers = Vec::new();
```

becomes:

```rust
        let mut registers = Vec::new_in(self.frame);
```

and

```rust
        let mut shareable: Vec<(LocalId, ir::StackSlot)> = Vec::new();
```

becomes:

```rust
        let mut shareable: Vec<(LocalId, ir::StackSlot), &Frame> = Vec::new_in(self.frame);
```

`shareable` is passed on as `&shareable` (a `&[_]` by deref). `forwarded` stays an `FxHashMap`:
`note_shareable` iterates it (13323), and moving it would change the order of `members`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `note_shareable` (13298-13348)

Before (13305-13328):

```rust
        let mut registers: Vec<LocalId> = self.registers.keys().copied().collect();
        blaze_ids::radix::sort_copy_by_key(&mut registers, |it| it.into_raw().into_u32());
        let count = shareable.len() + registers.len();
        if count < 2 {
            return;
        }
        let at: FxHashMap<LocalId, usize> =
            shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)).collect();
        // Each local with the slot it is kept in, by the slot's index.
        let mut members: Vec<(LocalId, usize)> =
            shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)).collect();
        let lazy: FxHashMap<LocalId, usize> = registers
            .iter()
            .enumerate()
            .map(|(at, &local)| (local, shareable.len() + at))
            .collect();
        members.extend(registers.iter().map(|local| (*local, lazy[local])));
        for (&temp, to) in forwarded {
            if let Some(&slot) = at.get(to) {
                members.push((temp, slot));
            }
        }
        let locals: Vec<LocalId> = members.iter().map(|it| it.0).collect();
        // A debugger reads ...
        let named: FxHashSet<LocalId> = match Self::names_are_written(body) {
            true => body.binding_locals.iter().map(|it| *it.1).collect(),
            false => FxHashSet::default(),
        };
        let storage = body.frame_storage_conflicts(&locals, |local| named.contains(&local));
```

After:

```rust
        let frame = self.frame;
        // In index order: what sorting the keys of a hash map gave.
        let mut registers: Vec<LocalId, &Frame> = Vec::with_capacity_in(self.registers.len(), frame);
        registers.extend(self.registers.keys());
        let count = shareable.len() + registers.len();
        if count < 2 {
            return;
        }
        let mut at: FrameHashMap<'_, LocalId, usize> =
            frame_map::hash_map_with_capacity(shareable.len(), frame);
        at.extend(shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)));
        // Each local with the slot it is kept in, by the slot's index.
        let mut members: Vec<(LocalId, usize), &Frame> =
            Vec::with_capacity_in(count + forwarded.len(), frame);
        members.extend(shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)));
        // alloc: kept in the recorder's frame locals (`overlap::FrameLocals`) past the build
        let lazy: FxHashMap<LocalId, usize> = registers
            .iter()
            .enumerate()
            .map(|(at, &local)| (local, shareable.len() + at))
            .collect();
        members.extend(registers.iter().map(|local| (*local, lazy[local])));
        for (&temp, to) in forwarded {
            if let Some(&slot) = at.get(to) {
                members.push((temp, slot));
            }
        }
        let locals: &[LocalId] = frame.alloc_slice_from_iter(members.iter().map(|it| it.0));
        // A debugger reads ...
        let mut named: FrameHashSet<'_, LocalId> = frame_map::hash_set(frame);
        if Self::names_are_written(body) {
            named.extend(body.binding_locals.iter().map(|it| *it.1));
        }
        let storage = body.frame_storage_conflicts(locals, |local| named.contains(&local));
```

The rest (13336-13347: `conflicts`, `slots`, `frame_locals`) is unchanged; `slots` and `lazy` are
kept in `cg.frame_locals`. `frame_storage_conflicts` takes `locals: &[LocalId]`
(`crates/hir-ty-mir/src/mir/storage.rs:56`).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### The test at 20005-20031

```rust
        let kept = KeptLayouts::new();
        let mut given = Vec::new();
        ...
        for (layout, given) in lent.into_iter().zip(given) {
            assert!(std::ptr::eq(layout, &*given));
            assert_eq!(*layout, *given);
        }
```

The body from `let kept` to the end goes inside a task:

```rust
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let kept = KeptLayouts::new_in(frame);
            let mut given = Vec::new();
            ...
            for (layout, given) in lent.into_iter().zip(given) {
                assert!(std::ptr::eq(layout, &*given));
                assert_eq!(*layout, *given);
            }
        });
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.5 Per-worker lists of relocations and line rows

> applied by codegen-v1 (2026-10-29): as written, with `emit_lent(ctx, speed)` as codegen-v2 asked and `emit_at` gone; `emit` is `#[cfg(test)]` (only the tests call it), `Relocs::of_mach` is gone (no caller once `emit_with` uses `extend_mach`), and `spare.rs` also gained the layout-list shelf of 1.4 and its test.

For each function v2 compiles, Cranelift's emit builds a `fragment::Relocs` and a
`Vec<LineRow>`. `convert::function` reads them into the kept `CompiledFunction` and drops them.
They become per-worker buffers: taken from the thread, written, read, and given back cleared.

They travel inside `fragment::Code` from `crate::emit` to `convert`. A `scratch::with_*` closure
can't span that, and a `scratch::Lent` would have to be a field of `Code`, which v1 keeps in salsa
memos. So they go on `spare.rs`'s shelf, the crate's per-worker store for Cranelift contexts.
v1 and LLVM still make exact-sized lists: v1's `Code` is kept in a memo, so a reused list's
capacity would be held for good.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `fragment.rs`, `impl Relocs` (309-342)

Before:

```rust
    /// Cranelift's relocations of `func`'s code.
    pub(super) fn of_mach(
        relocs: &[FinalizedMachReloc],
        func: &ir::Function,
    ) -> std::result::Result<Relocs, RelocError> {
        let mut rows = Relocs::with_capacity(relocs.len());
        for reloc in relocs {
            let name = match &reloc.target { ... };
            let (offset, kind, addend) = (reloc.offset, reloc.kind, reloc.addend);
            rows.push(Reloc { offset, kind, name, addend, elf_type: None })?;
        }
        Ok(rows)
    }
```

After:

```rust
    /// Cranelift's relocations of `func`'s code.
    pub(super) fn of_mach(
        relocs: &[FinalizedMachReloc],
        func: &ir::Function,
    ) -> std::result::Result<Relocs, RelocError> {
        let mut rows = Relocs::with_capacity(relocs.len());
        rows.extend_mach(relocs, func)?;
        Ok(rows)
    }

    /// Cranelift's relocations of `func`'s code after the rows it has, its rows grown once to
    /// hold them: what v2's emit writes into the thread's kept list ([`crate::spare::relocs`]).
    pub(super) fn extend_mach(
        &mut self,
        relocs: &[FinalizedMachReloc],
        func: &ir::Function,
    ) -> std::result::Result<(), RelocError> {
        self.rows.reserve_exact(relocs.len());
        for reloc in relocs {
            let name = match &reloc.target { ... }; // the match of `of_mach`, unchanged
            let (offset, kind, addend) = (reloc.offset, reloc.kind, reloc.addend);
            self.push(Reloc { offset, kind, name, addend, elf_type: None })?;
        }
        Ok(())
    }

    /// Empties it, keeping its room.
    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.wide.clear();
        self.spaces.clear();
    }

    /// The rows it has room for.
    pub(super) fn capacity(&self) -> usize {
        self.rows.capacity()
    }
```

`push` keys `wide` and `spaces` by the row's place, `rows.len()`, so rows pushed after `clear`
are the rows a new list gets. The derived `PartialEq` and `Hash` compare contents, not capacity.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `debuginfo.rs`, `LineRow::of_ranges` (90-104)

Before:

```rust
    pub(super) fn of_ranges<I>(ranges: I) -> Vec<LineRow>
    where
        I: IntoIterator<Item = (CodeOffset, ir::SourceLoc), IntoIter: Clone>,
    {
        let ranges = ranges.into_iter();
        let rows = || { ... };
        let mut out = Vec::with_capacity(rows().count());
        out.extend(rows());
        out
    }
```

After:

```rust
    pub(super) fn of_ranges<I>(ranges: I) -> Vec<LineRow>
    where
        I: IntoIterator<Item = (CodeOffset, ir::SourceLoc), IntoIter: Clone>,
    {
        let mut out = Vec::new();
        Self::extend_ranges(&mut out, ranges);
        out
    }

    /// [`Self::of_ranges`] onto the end of `out`, grown once to its exact length: what v2's
    /// emit writes into the thread's kept rows ([`crate::spare::lines`]).
    pub(super) fn extend_ranges<I>(out: &mut Vec<LineRow>, ranges: I)
    where
        I: IntoIterator<Item = (CodeOffset, ir::SourceLoc), IntoIter: Clone>,
    {
        let ranges = ranges.into_iter();
        let rows = || {
            let mut last = None;
            ranges.clone().filter_map(move |(offset, loc)| {
                let span = Self::span_of(loc)?;
                (last.replace(span) != Some(span)).then_some(LineRow { offset, span })
            })
        };
        out.reserve_exact(rows().count());
        out.extend(rows());
    }
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `spare.rs`

After line 16 (`use cranelift_frontend::FunctionBuilderContext;`):

```rust

use crate::{debuginfo::LineRow, fragment::Relocs};
```

After `const LARGEST` (line 23):

```rust

/// A kept list of relocations or line rows with room for more than this is let go rather than
/// kept, as a context of more than [`LARGEST`] instructions is.
const LARGEST_ROWS: usize = 1 << 16;
```

The `thread_local!` (25-28) gains two shelves:

```rust
    static RELOCS: RefCell<Vec<Relocs>> = const { RefCell::new(Vec::new()) };
    static LINES: RefCell<Vec<Vec<LineRow>>> = const { RefCell::new(Vec::new()) };
```

After `keep_builder` (line 61):

```rust
/// An empty list of relocations: one the thread kept, or a new one. v2's emit writes a
/// function's into it ([`super::emit_lent`]) and its conversion gives it back.
pub(super) fn relocs() -> Relocs {
    RELOCS.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps `relocs` for the thread's next function, emptied.
pub(super) fn keep_relocs(mut relocs: Relocs) {
    if relocs.capacity() > LARGEST_ROWS {
        return;
    }
    relocs.clear();
    RELOCS.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(relocs);
        }
    });
}

/// An empty list of line rows: one the thread kept, or a new one, as [`relocs`].
pub(super) fn lines() -> Vec<LineRow> {
    LINES.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps `lines` for the thread's next function, emptied.
pub(super) fn keep_lines(mut lines: Vec<LineRow>) {
    if lines.capacity() > LARGEST_ROWS {
        return;
    }
    lines.clear();
    LINES.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(lines);
        }
    });
}
```

`spare.rs`'s tests gain:

```rust
    /// A kept list comes back empty with its room, and filling it again to what it held takes
    /// nothing of the global allocator.
    #[test]
    fn a_kept_list_of_rows_is_reused_whole() {
        use crate::fragment::{Reloc, RelocName};
        LINES.with_borrow_mut(Vec::clear);
        RELOCS.with_borrow_mut(Vec::clear);
        let row = |at: u32| Reloc {
            offset: at,
            kind: cranelift_codegen::binemit::Reloc::Abs8,
            name: RelocName::Func(at),
            addend: 0,
            elf_type: None,
        };
        let fill = || {
            let mut lines = lines();
            let mut relocs = relocs();
            assert!(lines.is_empty() && relocs.len() == 0);
            lines.extend((0..100).map(|offset| LineRow { offset, span: offset }));
            for at in 0..100 {
                relocs.push(row(at)).expect("a row");
            }
            let made = (lines.len(), relocs.len());
            keep_lines(lines);
            keep_relocs(relocs);
            made
        };
        let cold = fill();
        let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's relocations and line rows", fill);
        assert_eq!(cold, warm);
        assert_eq!(warm, (100, 100));
    }
```

The test assumes `Relocs::push`'s signature as `of_mach` uses it (`rows.push(Reloc { .. })?`) and
that `RelocName::Func(u32)` exists (`v2/shape.rs:94` matches it).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `mod.rs`, `emit`, `emit_cached` and `emit_with` (5346-5416)

Before:

```rust
fn emit(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    emit_with(ctx, None)
}
...
fn emit_cached(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    emit_with(ctx, store::Store::global())
}
...
fn emit_with(
    ctx: &mut cranelift_codegen::Context,
    store: Option<&'static store::Store>,
) -> Result<Code> {
```

After:

```rust
fn emit(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    // alloc: none until the code's rows are written, then exactly that many: v1 keeps them
    emit_with(ctx, None, Relocs::default(), Vec::new())
}

/// [`emit`] into the thread's kept lists of relocations and line rows ([`spare::relocs`],
/// [`spare::lines`]): for v2, whose conversion gives them back ([`spare::keep_relocs`]).
fn emit_lent(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    emit_with(ctx, None, spare::relocs(), spare::lines())
}
...
fn emit_cached(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    // alloc: none until the code's rows are written, then exactly that many: v1 keeps them
    emit_with(ctx, store::Store::global(), Relocs::default(), Vec::new())
}
...
/// Runs Cranelift on a finished function, with `store` as its incremental cache, writing its
/// relocations and line rows into `relocs` and `lines`, which are empty. Cranelift panics on IR
/// it has no lowering for (i128 corners, for one); that must cost us one function, not the
/// process.
fn emit_with(
    ctx: &mut cranelift_codegen::Context,
    store: Option<&'static store::Store>,
    relocs: Relocs,
    lines: Vec<LineRow>,
) -> Result<Code> {
```

In its closure, before:

```rust
        Ok(Code {
            signature: ctx.func.signature.clone(),
            bytes: compiled.buffer.data().to_vec(),
            align: compiled.buffer.alignment.into(),
            relocs: Relocs::of_mach(compiled.buffer.relocs(), &ctx.func)
                .map_err(|e| e.to_string())?,
            unwind: FunctionUnwind::of(isa, ctx)?,
            lines: LineRow::of_ranges(
                compiled.buffer.get_srclocs_sorted().iter().map(|it| (it.start, it.loc)),
            ),
            slots: SlotPlaces::default(),
        })
```

After:

```rust
        let mut relocs = relocs;
        relocs.extend_mach(compiled.buffer.relocs(), &ctx.func).map_err(|e| e.to_string())?;
        let mut lines = lines;
        LineRow::extend_ranges(
            &mut lines,
            compiled.buffer.get_srclocs_sorted().iter().map(|it| (it.start, it.loc)),
        );
        Ok(Code {
            // clone: the kept code's signature, a few parameters
            signature: ctx.func.signature.clone(),
            // alloc: the kept code, written once at its exact length
            bytes: compiled.buffer.data().to_vec(),
            align: compiled.buffer.alignment.into(),
            relocs,
            unwind: FunctionUnwind::of(isa, ctx)?,
            lines,
            slots: SlotPlaces::default(),
        })
```

Errors come in the same order: relocations, then unwind information. The closure takes `relocs`
and `lines` by value (it is `FnOnce`, under `AssertUnwindSafe`). If `ctx.func` is for wasm,
`emit_with` returns `emit_wasm(..)` first and the lists are dropped; v2 refuses wasm units.

`LineRow` and `Relocs` are already imported in `mod.rs`: `emit_with` names both.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.6 `v2/places.rs`: a unit's emit on one frame

> applied by codegen-v2 (2026-10-29): `Placer<'a, A>` with `listed_in`, the generic `place_requests`, `resolve`/`resolve_owning` in a `Codegen` task with `Placed::frame`, the frame index of symbols and `libcall_name`, as written; its test is in a new `mod tests` of `v2/places.rs` (not `v2/tests.rs`, which codegen-v2-mem-v2a doesn't own), with one more test of the frame index.

`resolve` and `resolve_owning` are where one unit's emit runs (`told` and the LLVM path). Each
opens a task, and `Placed` lends its frame to `convert` and `side`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Placer` (165-237): its index in a frame for v2's prepare

Line 5-9's `use std::{..}` gains `alloc::{Allocator, Global},`.

```rust
pub(crate) struct Placer<'a> {
    refs: Cow<'a, [Ref]>,
    index: HashTable<u32>,
    open: bool,
}
```

becomes:

```rust
pub(crate) struct Placer<'a, A: Allocator = Global> {
    refs: Cow<'a, [Ref]>,
    /// ... (unchanged doc) In `A`: a unit's frame when it is a function's.
    index: HashTable<u32, A>,
    open: bool,
}
```

`impl<'a> Placer<'a> {` (186) splits in two:

```rust
impl<'a> Placer<'a> {
    pub(crate) fn listed(refs: impl Into<Cow<'a, [Ref]>>) -> Placer<'a> {
        Placer::listed_in(refs, Global)
    }

    pub(crate) fn open() -> Placer<'a> {
        Placer { refs: Cow::Owned(Vec::new()), index: HashTable::new(), open: true }
    }
}

impl<'a, A: Allocator> Placer<'a, A> {
    /// [`Placer::listed`] with its index in `alloc`: a unit's frame.
    pub(crate) fn listed_in(refs: impl Into<Cow<'a, [Ref]>>, alloc: A) -> Placer<'a, A> {
        let refs = refs.into();
        let mut index = HashTable::with_capacity_in(refs.len(), alloc);
        for (place, it) in refs.iter().enumerate() {
            let hashed = hash(it);
            if index.find(hashed, |&at: &u32| refs[at as usize] == *it).is_none() {
                index.insert_unique(hashed, place as u32, |&at| hash(&refs[at as usize]));
            }
        }
        Placer { refs, index, open: false }
    }

    // `place`, `symbol`, `add`, `refs` and `into_refs`, unchanged
}
```

`place_requests` (246) takes any placer:

```rust
pub(crate) fn place_requests<A: Allocator>(
    db: &Db,
    homes: &mut Homes,
    hashes: &dyn Hashes,
    krate: Option<Crate>,
    placer: &mut Placer<'_, A>,
    requests: &mut Requests,
) -> Result<Places, String> {
```

Every other caller (`v2/statics.rs:45`, the tests) keeps `Placer::listed`/`Placer::open` and
`Global`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `resolve`, `resolve_owning` and `Placed` (348-420)

Before:

```rust
pub(crate) fn resolve<T>(places: &Places, refs: &Refs, work: impl FnOnce(&Placed<'_>) -> T) -> T {
    work(&Placed { places, refs, symbols: OnceCell::new(), own: None })
}
...
) -> (T, Vec<String>) {
    let placed = Placed { places, refs, symbols: OnceCell::new(), own: Some(RefCell::default()) };
    let done = work(&placed);
    (done, placed.own.map(RefCell::into_inner).unwrap_or_default())
}
...
pub(crate) struct Placed<'a> {
    places: &'a Places,
    refs: &'a Refs,
    symbols: OnceCell<HashTable<(u64, u32)>>,
    /// The symbols the backend named that `refs` does not list, when it may name its own.
    own: Option<RefCell<Vec<String>>>,
}
```

After:

```rust
/// Resolves a function's requests against the targets of its places, in a frame of the
/// worker's that `work` gets through [`Placed::frame`] and that is reset when it returns.
pub(crate) fn resolve<T>(places: &Places, refs: &Refs, work: impl FnOnce(&Placed<'_>) -> T) -> T {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        work(&Placed { places, refs, frame, symbols: OnceCell::new(), own: None })
    })
}
...
) -> (T, Vec<String>) {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        let own = Some(RefCell::default());
        let placed = Placed { places, refs, frame, symbols: OnceCell::new(), own };
        let done = work(&placed);
        (done, placed.own.map(RefCell::into_inner).unwrap_or_default())
    })
}
...
pub(crate) struct Placed<'a> {
    places: &'a Places,
    refs: &'a Refs,
    /// The unit's frame: its emit's tables are kept there.
    frame: &'a Frame,
    symbols: OnceCell<HashTable<(u64, u32), &'a Frame>>,
    /// The symbols the backend named that `refs` does not list, when it may name its own. Kept
    /// past the emit ([`blaze_codegen::backend::Emitted::extra`]).
    own: Option<RefCell<Vec<String>>>,
}

impl<'a> Placed<'a> {
    /// The frame the unit's emit runs in.
    pub(crate) fn frame(&self) -> &'a Frame {
        self.frame
    }
}
```

In `symbol` (409-420):

```rust
            let mut index: HashTable<(u64, u32)> = HashTable::new();
```

becomes:

```rust
            let mut index: HashTable<(u64, u32), &Frame> = HashTable::new_in(self.frame);
```

`use blaze_mem::Frame;` joins the imports.

- **`T` can't borrow the frame.** `work`'s result is a type fixed by the caller, so nothing that
  borrows `Placed<'_>` gets out (the `for<'_>` bound), and `frame::task` forbids it too.
- **Lifetimes.** `OnceCell` makes `Placed<'a>` invariant. The references given (`places`, `refs`
  and `frame`) shorten to one local lifetime inside the closure.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `libcall_symbol` (154-156): no `Box` and `String` per libcall relocation

`cranelift_module::default_libcall_names()` (cranelift-module 0.136.2, `src/lib.rs:39`) returns
`Box<dyn Fn(ir::LibCall) -> String>`. `convert` called it for every libcall relocation, making a
`Box` and a `String` each time. Add beside it:

```rust
/// The C library's name of each routine Cranelift calls itself, made once a process:
/// `default_libcall_names` makes a boxed closure and a `String` a call.
pub(crate) fn libcall_name(libcall: ir::LibCall) -> Result<&'static str, String> {
    static NAMES: std::sync::LazyLock<Box<[(ir::LibCall, String)]>> = std::sync::LazyLock::new(|| {
        let name = cranelift_module::default_libcall_names();
        // alloc: once a process
        ir::LibCall::all_libcalls().iter().map(|&it| (it, name(it))).collect()
    });
    match NAMES.iter().find(|it| it.0 == libcall) {
        Some((_, name)) => Ok(name.as_str()),
        None => Err(format!("{libcall:?}, which Cranelift's list of its libcalls lacks")),
    }
}
```

`ir::LibCall::all_libcalls() -> &'static [LibCall]` and `LibCall: Copy + Eq` are in
cranelift-codegen 0.136.2 (`src/ir/libcall.rs:20`, `:103`). `libcall_symbol` stays as it is for
`libcall_ref`, which keeps the name in a `FrontRef`. Test, in `v2/tests.rs` (`v2/places.rs`
has no test module):

```rust
#[test]
fn every_libcall_is_named_as_cranelift_names_it() {
    let name = cranelift_module::default_libcall_names();
    for &libcall in cranelift_codegen::ir::LibCall::all_libcalls() {
        assert_eq!(super::places::libcall_name(libcall), Ok(name(libcall).as_str()));
    }
}
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.7 `v2/convert.rs`

> applied by codegen-v2 (2026-10-29): `function(frame, ..)` and `value(frame, ..)`, `used` and `targets` in the frame, the lists handed back through `spare::keep_relocs`/`keep_lines` (1.5, codegen-v1's), `libcall_name` and the test helper in a task, as written; the budget test bounds a warm conversion at 10, not 2, because the kept relocations are now a `blaze_codegen::Relocs` table (rows and targets) and building it through `FromIterator` and `try_map` takes 7 temporaries of `relocs.rs`'s own on the global heap.

`function` and `value` take their frame from the caller's `Placed`, so the per-unit tables are
in the unit's frame and no second task opens.

`use blaze_mem::Frame;` joins the imports. Line 12,
`use super::places::{PERSONALITY, libcall_symbol};`, becomes
`use super::places::{PERSONALITY, libcall_name};`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `function` (78-140)

Signature:

```rust
pub(crate) fn function(
    frame: &Frame,
    key: InstanceKey,
    arch: Arch,
    isa: &dyn TargetIsa,
    code: fragment::Code,
    requests: &Requests,
    resolve: &impl Resolver,
    spans: &[SpanRef],
    side: Option<ContentId>,
) -> Result<CompiledFunction, String> {
```

Line 94: `let datas = own.requests(requests, resolve, named)?;` becomes
`let datas = own.requests(frame, requests, resolve, named)?;`.

The end (123-139), before:

```rust
    let align = u32::try_from(code.align).map_err(|e| e.to_string())?;
    if !align.is_power_of_two() {
        return Err(format!("code aligned to {align}, not a power of two"));
    }
    Ok(CompiledFunction {
        key,
        arch,
        align,
        code: code.bytes,
        relocs,
        data: own.data,
        inlined: Vec::new(),
        coverage: None,
        side,
        unwind,
        lines,
    })
```

after:

```rust
    let align = u32::try_from(code.align).map_err(|e| e.to_string())?;
    if !align.is_power_of_two() {
        return Err(format!("code aligned to {align}, not a power of two"));
    }
    let fragment::Code { bytes, relocs: rows, lines: line_rows, .. } = code;
    crate::spare::keep_relocs(rows);
    crate::spare::keep_lines(line_rows);
    Ok(CompiledFunction {
        key,
        arch,
        align,
        code: bytes,
        relocs,
        data: own.data,
        inlined: Vec::new(),
        coverage: None,
        side,
        unwind,
        lines,
    })
```

The kept parts are allocated once each at their exact length: `relocs`
(`Vec::with_capacity(code.relocs.len())`, 95), `lines` (`Vec::with_capacity(code.lines.len())`,
115) and the own data. A conversion that fails drops its lists instead of keeping them; the
thread makes new ones next time.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `value` (143-155)

```rust
pub(crate) fn value(
    frame: &Frame,
    arch: Arch,
    blob: &ConstBlob,
    requests: &Requests,
    resolve: &impl Resolver,
) -> Result<(Vec<Data>, Vec<Reloc>), String> {
    let mut own = OwnData { data: Vec::new(), word: word(arch)? };
    let named = blob.relocs.iter().map(|&(_, data, _)| data.as_u32() as usize);
    let targets = own.requests(frame, requests, resolve, named)?;
    ...
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `OwnData::requests` (181-228)

Before:

```rust
    fn requests(
        &mut self,
        requests: &Requests,
        resolve: &impl Resolver,
        named: impl Iterator<Item = usize>,
    ) -> Result<Vec<Option<Target>>, String> {
        let mut used = vec![false; requests.datas.len()];
        ...
        let mut targets: Vec<Option<Target>> = Vec::with_capacity(requests.datas.len());
```

After:

```rust
    fn requests<'f>(
        &mut self,
        frame: &'f Frame,
        requests: &Requests,
        resolve: &impl Resolver,
        named: impl Iterator<Item = usize>,
    ) -> Result<Vec<Option<Target>, &'f Frame>, String> {
        let used: &mut [bool] = frame.alloc_slice_fill_with(requests.datas.len(), |_| false);
        ...
        let mut targets: Vec<Option<Target>, &Frame> = Vec::with_capacity_in(requests.datas.len(), frame);
```

The rest of the body is unchanged: `used[..]`, `used.get_mut(..)`, `&targets` as
`&[Option<Target>]`. The `Vec` in the frame drops its `Target`s (an `Extern` owns a `String`)
when it drops, so nothing leaks. Only `Frame::alloc` values skip their destructors, and `used`'s
`bool`s have none.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `reloc_target` (342)

```rust
        RelocName::LibCall(libcall) => resolve.symbol(&libcall_symbol(libcall)),
```

becomes:

```rust
        RelocName::LibCall(libcall) => resolve.symbol(libcall_name(libcall)?),
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### Test helper, `v2/convert/tests.rs:66-80`

```rust
    function(key(), Arch::X86_64, isa, code, requests, &resolve, &[span(0), span(1)], None)
```

becomes:

```rust
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        function(frame, key(), Arch::X86_64, isa, code, requests, &resolve, &[span(0), span(1)], None)
    })
```

Budget test, in `v2/convert/tests.rs`:

```rust
/// A warm conversion allocates what the function keeps and nothing else: here its
/// relocations and its line rows. Its code moves in, its tables are the frame's, and the
/// lists Cranelift's emit wrote go back to the thread.
#[test]
fn a_warm_conversion_allocates_only_what_it_keeps() {
    let make = || {
        code(
            vec![reloc(1, binemit::Reloc::X86CallPLTRel4, 0, 0)],
            vec![LineRow { offset: 0, span: 0 }, LineRow { offset: 4, span: 1 }],
        )
    };
    let requests = requests();
    convert(make(), &requests).expect("converted");
    let code = make();
    let compiled = blaze_alloc_count::assert_at_most(2, "a warm conversion: its relocations and its lines", || {
        convert(code, &requests)
    })
    .expect("converted");
    assert_eq!(compiled.relocs.len(), 1);
    assert_eq!(compiled.lines.len(), 2);
    assert!(compiled.data.is_empty());
}
```

Why the count is 2. Function 0 resolves to `callee()`, a `Target::Function`, which allocates
nothing. No data is named, so `used` and `targets` (frame) are all `false` and `None`. `code`
moves in. `keep_relocs` and `keep_lines` push onto shelves the cold run already made.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.8 `v2/backend.rs`

> applied by codegen-v2 (2026-10-29): all of 1.8, by codegen-v2-mem-v2b, keeping `Backend::Cranelift { speed }` and `Bits` (see `colocate` and `compiled`).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### Imports (31-32)

> applied by codegen-v2 (2026-10-29): `rustc_hash` and `smallvec` go and `blaze_mem::Frame` and `crate::frame_map::{self, FrameHashMap}` come in; `blaze_ids::bits::Bits` stays, `colocate` uses it.

```rust
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
```

Both go: after the changes below nothing in the file uses them (`rg -n 'FxHash|SmallVec'
v2/backend.rs` lists only lines 446-545). Add:

```rust
use blaze_mem::Frame;

use crate::frame_map::{self, FrameHashMap};
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Backends::prepared` (152-240): one frame for the prepare of one instance

> applied by codegen-v2 (2026-10-29): the body runs in one `Codegen` task, with the four calls taking the frame and the `// alloc:` note, as written; the `match` keeps the tree's `Backend::Cranelift { .. }`.

Its body (162-239) moves, unchanged except for the four calls below, into a task:

```rust
    ) -> Result<(Shape, Prepared), CodegenError> {
        // What preparing one instance makes and lets go of (the placer's index, the scope
        // tables, the shape's tables) is in a frame of the worker's, reset when it returns;
        // what it keeps (the IR, its places, spans, variables and scopes) is the heap's.
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let fail = |what: String| failed(unit, format!("{instance:?}: {what}"));
            ... // 163-233 as they are, but:
            //   179: let mut placer = Placer::listed_in(walked, frame);
            //   206: named(db, hashes, krate, source.owner, &variables, &source.spans, frame).map_err(fail)?
            //   218: colocate(&mut pending.func, &places, placer.refs(), frame);
            //   234: let shape = shape::of(unit, &function, frame).map_err(fail)?;
            let prepared = match unit.backend {
                Backend::Cranelift => Prepared::Cranelift(Box::new(function)),
                Backend::Llvm { .. } => Prepared::Llvm(Box::new(function)),
            };
            Ok((shape, prepared))
        })
    }
```

- **Early returns.** The `return Err(..)`s at 165 and 173 return from the closure, which returns
  `Result<(Shape, Prepared), CodegenError>` like the function.
- **Nesting.** `built(..)` (180) builds the IR through `mod.rs` `build()`, whose task nests inside
  this one.
- **What escapes.** `Function` and `Shape` hold nothing of the frame: `walked` is borrowed salsa
  data, `Placer::listed_in` only indexes it in the frame, and `placer` is dropped in the closure.
- **The `Box`.** `Box::new(function)` (236-237) is the kept prepared function; add
  `// alloc: the prepared function, kept by the driver until its emit` above the `match`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `named` (410-434)

> applied by codegen-v2 (2026-10-29): as written.

Before:

```rust
fn named(
    db: &Db,
    hashes: &dyn Hashes,
    krate: Crate,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
    spans: &[MirSpan],
) -> Result<(Vec<side::Variable>, side::Scopes), String> {
    let body = Body::of(db, owner);
    let variables: Vec<_> =
        variables.iter().filter(|it| !body[it.binding].name.as_str().starts_with('<')).collect();
    let (scopes, of_binding) = scoped(db, owner, &variables, spans);
    let mut named = Vec::with_capacity(variables.len());
    for variable in variables {
```

After:

```rust
fn named(
    db: &Db,
    hashes: &dyn Hashes,
    krate: Crate,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
    spans: &[MirSpan],
    frame: &Frame,
) -> Result<(Vec<side::Variable>, side::Scopes), String> {
    let body = Body::of(db, owner);
    let mut told: Vec<&fragment::Variable, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    told.extend(variables.iter().filter(|it| !body[it.binding].name.as_str().starts_with('<')));
    let (scopes, of_binding) = scoped(db, owner, &told, spans, frame);
    // alloc: the named locals, kept in the prepared function
    let mut named = Vec::with_capacity(told.len());
    for variable in told.iter().copied() {
```

The loop body is unchanged (`of_binding.get(&variable.binding).copied()` is hashbrown's `get`).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `scoped` (441-518)

> applied by codegen-v2 (2026-10-29): as written: `wanted` of raw indices sorted in the thread's `u32` buffer, the maps and the reused `chain` in the frame.

Before (441-492, the rest unchanged):

```rust
fn scoped(
    db: &Db,
    owner: DefWithBodyId,
    variables: &[&fragment::Variable],
    spans: &[MirSpan],
) -> (side::Scopes, FxHashMap<BindingId, u32>) {
    ...
    let mut wanted: SmallVec<[BindingId; 16]> = variables.iter().map(|it| it.binding).collect();
    blaze_ids::radix::sort_copy_by_key(&mut wanted, |it| it.into_raw().into_u32());
    wanted.dedup();
    let mut bound = FxHashMap::with_capacity_and_hasher(wanted.len(), Default::default());
    let mut seen = FxHashSet::default();
    'scopes: for ... { ... }
    let mut told: FxHashMap<ScopeId, u32> = FxHashMap::default();
    let mut parents = Vec::new();
    let mut of_binding = FxHashMap::default();
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        let mut chain: Vec<ScopeId> = scopes
            .scope_chain(Some(scope))
            .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
            .filter(|&it| it == scope || !scopes.entries(it).is_empty())
            .collect();
        chain.reverse();
        for scope in chain {
            ...
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return (side::Scopes::default(), of_binding);
    }
    let mut tried = FxHashMap::default();
```

After:

```rust
fn scoped<'f>(
    db: &Db,
    owner: DefWithBodyId,
    variables: &[&fragment::Variable],
    spans: &[MirSpan],
    frame: &'f Frame,
) -> (side::Scopes, FrameHashMap<'f, BindingId, u32>) {
    ...
    // By their raw index, which is what the sort keyed them by before: the radix sort's
    // buffer is the thread's.
    let mut wanted: Vec<u32, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    wanted.extend(variables.iter().map(|it| it.binding.into_raw().into_u32()));
    blaze_mem::scratch::with_u32s(|scratch| {
        blaze_ids::radix::sort_copy_by_key_in(&mut wanted, |&it| it, scratch);
    });
    wanted.dedup();
    let mut bound = frame_map::hash_map_with_capacity(wanted.len(), frame);
    let mut seen = frame_map::hash_set(frame);
    'scopes: for ... {
        ... // unchanged, but for the test at 459:
        //   if wanted.binary_search(&entry.binding().into_raw().into_u32()).is_ok() {
    }
    let mut told: FrameHashMap<'_, ScopeId, u32> = frame_map::hash_map(frame);
    // alloc: the scopes' parents, kept in the prepared function
    let mut parents = Vec::new();
    let mut of_binding = frame_map::hash_map(frame);
    // A binding's scopes not told of yet, outermost first: one list for the unit, refilled
    // for each binding.
    let mut chain: Vec<ScopeId, &Frame> = Vec::new_in(frame);
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        chain.clear();
        chain.extend(
            scopes
                .scope_chain(Some(scope))
                .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
                .filter(|&it| it == scope || !scopes.entries(it).is_empty()),
        );
        chain.reverse();
        for &scope in chain.iter() {
            ... // unchanged
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return (side::Scopes::default(), of_binding);
    }
    let mut tried = frame_map::hash_map(frame);
```

The rest is unchanged. `of_span` (508) is kept (`Vec::with_capacity(spans.len())`). The maps
here are only looked up, never iterated.

`wanted` holds each binding's raw index, sorted and deduplicated, and the membership test asks
for the same index. Before, it held the `BindingId`s sorted by that same key, so the same
bindings are wanted.

Why the sort changes: `blaze_radix::sort_copy_by_key` allocates its scratch on every call.
`sort_copy_by_key_in(items: &mut [T], key, scratch: &mut Vec<T>)`
(`crates/blaze-radix/src/lib.rs:196`) "allocates nothing" once `scratch` has the room. Its
scratch is `scratch::with_u32s`' buffer, which is why the list holds `u32`s. `&mut wanted`
derefs to `&mut [u32]`, and `binary_search` is the slice's. `u32` implements `blaze_radix::Key`
(`key!(.., u32, ..)` at `crates/blaze-radix/src/lib.rs:90`).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `colocate` (524-570)

> applied by codegen-v2 (2026-10-29): with the tree's `Bits`: `addressed` is `Bits<&mut [u64]>` over words `frame.alloc_slice_fill_with`'d zero, so `insert` and `contains` stay as they are.

```rust
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref]) {
    ...
    // One bit a callee: no allocation for the usual few hundred.
    let mut addressed: SmallVec<[u64; 4]> = SmallVec::new();
    addressed.resize(stencil.dfg.ext_funcs.len().div_ceil(64), 0);
```

becomes:

```rust
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref], frame: &Frame) {
    ...
    // One bit a callee, in the frame.
    let addressed: &mut [u64] = frame.alloc_slice_fill_with(stencil.dfg.ext_funcs.len().div_ceil(64), |_| 0);
```

The uses (`addressed[at / 64] |= ..`, `addressed[at / 64] & ..`) are unchanged.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `told` (630-663): the side table and the conversion in the emit's frame

> applied by codegen-v2 (2026-10-29): as written.

Before (646-662):

```rust
    let isa = isa(unit)?;
    let side = side::of(variables, scopes, &code).map_err(fail)?;
    let compiled = resolve(places, refs, |resolve| {
        super::convert::function(
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
        )
    })
    .map_err(fail)?;
    let function = CompiledFunction { inlined, ..compiled };
    Ok(Emitted { function, side: side.map(|it| it.0), extra: Vec::new() })
```

After:

```rust
    let isa = isa(unit)?;
    let (compiled, side) = resolve(places, refs, |resolve| {
        let side = side::of(variables, scopes, &code, resolve.frame())?;
        let compiled = super::convert::function(
            resolve.frame(),
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
        )?;
        Ok::<_, String>((compiled, side))
    })
    .map_err(fail)?;
    let function = CompiledFunction { inlined, ..compiled };
    Ok(Emitted { function, side: side.map(|it| it.0), extra: Vec::new() })
```

Errors and their order are the same: the side table's first, then the conversion's, each through
`fail`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `compiled` (697-714)

> applied by codegen-v2 (2026-10-29): the tree compiles with `crate::emit_at(context, speed)` now, so the call is `crate::emit_lent(context, speed)`; codegen-v1's `emit_lent` (1.5) must take `speed` and pass it to `emit_with` as `emit_at` does.

```rust
        let mut code = crate::emit(context).map_err(|e| failed(unit, e))?;
```

becomes:

```rust
        let mut code = crate::emit_lent(context).map_err(|e| failed(unit, e))?;
```

Every `Code` from `compiled()` goes to `told` and then to `convert::function`, which gives its
lists back: `emit_cranelift`, `emit_ir`'s body and thunks.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.9 `v2/shape.rs`

> applied by codegen-v2 (2026-10-29): all of 1.9, by codegen-v2-mem-v2b; the helpers are spelled `&mut Out<'_>`, since the workspace warns on `elided_lifetimes_in_paths`.

The shape's bytes are hashed into its key and never kept, so they are the thread's scratch
buffer. Its data names are in the frame.

`use blaze_codegen::backend::Shape;` (13) becomes `use blaze_codegen::{Unit, backend::Shape};`.
Add `use blaze_mem::Frame;`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `of` (51-134)

> applied by codegen-v2 (2026-10-29): as written; it ends in `Shape::of(unit, out.0)`, which is 2.2's.

Before:

```rust
pub(crate) fn of(function: &Function) -> Result<Shape, String> {
    let places = &function.places;
    let requests = function.requests();
    let data = Data::of(places, requests)?;
    let mut out = Out::with_capacity(estimate(function));
    out.u8(VERSION);
    ...
    out.u8(part::UNSANITIZED);
    out.u8(function.unsanitized.0);
    Ok(Shape(out.0))
}
```

After:

```rust
pub(crate) fn of(unit: &Unit, function: &Function, frame: &Frame) -> Result<Shape, String> {
    let places = &function.places;
    let requests = function.requests();
    let data = Data::of(places, requests, frame)?;
    // The bytes are hashed into the shape's key ([`Shape::of`]) once written: the thread's
    // buffer's, never the shape's.
    blaze_mem::scratch::with_bytes(|bytes| {
        bytes.reserve(estimate(function));
        let mut out = Out(bytes);
        out.u8(VERSION);
        ... // 56-131 unchanged
        out.u8(part::UNSANITIZED);
        out.u8(function.unsanitized.0);
        Ok(Shape::of(unit, out.0))
    })
}
```

The `return Err(..)`s at 70-71 and 98 return from the closure, which returns
`Result<Shape, String>` like the function.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `value` (136-)

> applied by codegen-v2 (2026-10-29): as written.

```rust
pub(crate) fn value(blob: &ConstBlob, requests: &Requests, places: &Places) -> Result<Vec<u8>, String> {
    let data = Data::of(places, requests)?;
    let mut out = Out::with_capacity(64 + 2 * blob.bytes.len());
    ...
    Ok(out.0)
}
```

becomes:

```rust
pub(crate) fn value(blob: &ConstBlob, requests: &Requests, places: &Places) -> Result<Vec<u8>, String> {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        let data = Data::of(places, requests, frame)?;
        // alloc: the static's key bytes, returned to its caller
        let mut bytes = Vec::with_capacity(64 + 2 * blob.bytes.len());
        let mut out = Out(&mut bytes);
        ... // unchanged
        Ok(bytes)
    })
}
```

The last write before `Ok` is the body's last `out.` call; `out` is dead after it, so `bytes` is
free to return.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Data` (229-262)

> applied by codegen-v2 (2026-10-29): as written.

```rust
struct Data {
    names: Vec<UserExternalName>,
}

impl Data {
    fn of(places: &Places, requests: &Requests) -> Result<Data, String> {
        ...
        let (mut own, mut positions) = (0, 0);
        let names = places
            .datas
            .iter()
            .map(|place| match place { ... })
            .collect();
        Ok(Data { names })
    }
```

becomes:

```rust
struct Data<'f> {
    names: &'f [UserExternalName],
}

impl<'f> Data<'f> {
    fn of(places: &Places, requests: &Requests, frame: &'f Frame) -> Result<Data<'f>, String> {
        ...
        let (mut own, mut positions) = (0, 0);
        let names = frame.alloc_slice_from_iter(places.datas.iter().map(|place| match place { ... }));
        Ok(Data { names })
    }
```

The match is unchanged. It runs once per place, in order, so the numbering of `own` and
`positions` is the same. `name` is unchanged.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Out` (179-221)

> applied by codegen-v2 (2026-10-29): as written, with the helpers spelled `&mut Out<'_>`.

```rust
struct Out(Vec<u8>);

impl Out {
    fn with_capacity(capacity: usize) -> Out {
        Out(Vec::with_capacity(capacity))
    }
```

becomes:

```rust
struct Out<'b>(&'b mut Vec<u8>);

impl Out<'_> {
```

(`with_capacity` goes.) `impl Hasher for Out {` becomes `impl Hasher for Out<'_> {`. The method
bodies are unchanged (`self.0.push`, `self.0.extend_from_slice`, `self.0.len()`). Helpers taking
`out: &mut Out` keep that spelling; the elided lifetime is inferred.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.10 `v2/side.rs`: one exact allocation for a side table

> applied by codegen-v2 (2026-10-29): all of 1.10, by codegen-v2-mem-v2b. The `Written*` twins are as written, except that `seq`'s bound is spelled `serde::Serialize`: `Serialize` in this file is `serde_derive`'s macro, and only `serde::Serializer` is imported.

`encode` builds `told` (a `Vec<Told>`, cloning every `Place::Ranges`), `scopes` and `inlined`
(each a `Vec` of `Vec`s) only to serialize them, then grows the output by doubling
(`postcard::to_allocvec`). Cargo's dev profile tells a debugger of variables, so this runs for
nearly every dev unit.

After the change:

- the lists are in the unit's frame;
- each place is borrowed from the `SlotPlaces`, except a pointee's, which is made for it;
- the bytes are written once, into one allocation of their exact size.

`Side` stays as it is: it is what `decode` reads. The writer is a twin with the same fields in the
same order. Postcard writes a struct as its fields in order (no names), a `Vec<T>`, a `&[T]` and a
`Cow<[T]>` alike as a length and the items, and an enum as its variant's index and fields. So the
twin's bytes are `Side`'s bytes; 1.13 tests it.

Imports: `use blaze_mem::Frame;` and `use serde::Serializer;` (`serde.workspace = true` is in
`Cargo.toml`).

After `Place` (127):

```rust
/// [`Side`] as [`encode`] writes it, its lists in the unit's frame: the same fields in the
/// same order, so postcard writes the bytes a `Side` of them would, and [`decode`] reads them
/// back as one.
#[derive(Serialize)]
struct Written<'a, 'f> {
    version: u32,
    frame_base: Cow<'a, [u8]>,
    #[serde(serialize_with = "seq")]
    scopes: Vec<WrittenScope<'f>, &'f Frame>,
    #[serde(serialize_with = "seq")]
    variables: Vec<WrittenTold<'a>, &'f Frame>,
    #[serde(serialize_with = "seq")]
    inlined: Vec<WrittenInlined<'f>, &'f Frame>,
}

/// [`Scope`] as written.
#[derive(Serialize)]
struct WrittenScope<'f> {
    parent: Option<u32>,
    #[serde(serialize_with = "seq")]
    ranges: Vec<(u32, u32), &'f Frame>,
}

/// [`Inlined`] as written.
#[derive(Serialize)]
struct WrittenInlined<'f> {
    parent: Option<u32>,
    call_site: SpanRef,
    #[serde(serialize_with = "seq")]
    ranges: Vec<(u32, u32), &'f Frame>,
}

/// [`Told`] as written: its place borrowed from the function's slots unless it is a pointee's.
#[derive(Serialize)]
struct WrittenTold<'a> {
    name: &'a str,
    param: bool,
    ty: ContentId,
    place: WrittenPlace<'a>,
    scope: Option<u32>,
}

/// [`Place`] as written, its variants in `Place`'s order.
#[derive(Serialize)]
enum WrittenPlace<'a> {
    Frame(i64),
    Ranges(Cow<'a, [(u32, u32, Vec<u8>)]>),
}

/// A list in a frame as serde writes a `Vec`: its length, then each item.
fn seq<T: Serialize, S: Serializer>(items: &Vec<T, &Frame>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(items.iter())
}
```

serde's `impl Serialize for Vec<T>` is `serializer.collect_seq(self)`. A slice iterator gives its
exact length, so postcard writes the length first, as for the `Vec`. `SpanRef: Serialize` is
already relied on (`Inlined` derives it). The import `use serde_derive::{Deserialize, Serialize};`
(line 11) already brings the derive.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `encode` (130-166)

> applied by codegen-v2 (2026-10-29): as written (the borrowed ranges are `Cow::Borrowed(&ranges[..])`, the buffer `Vec::<u8>::with_capacity(size)`).

After:

```rust
pub(crate) fn encode(
    variables: &[Variable],
    scopes: &Scopes,
    slots: &SlotPlaces,
    lines: &[LineRow],
    len: u32,
    frame: &Frame,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let mut told: Vec<WrittenTold<'_>, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    told.extend(variables.iter().filter_map(|variable| {
        let place = slots.of(variable.slot)?;
        let place = match (variable.pointer, place) {
            // alloc: a pointee's place, made for it
            (Some(offset), place) => match place.pointee(offset) {
                VariablePlace::Frame(offset) => WrittenPlace::Frame(offset),
                VariablePlace::Ranges(ranges) => WrittenPlace::Ranges(Cow::Owned(ranges)),
            },
            (None, VariablePlace::Frame(offset)) => WrittenPlace::Frame(*offset),
            (None, VariablePlace::Ranges(ranges)) => WrittenPlace::Ranges(Cow::Borrowed(ranges)),
        };
        Some(WrittenTold {
            name: variable.name.as_str(),
            param: variable.param,
            ty: variable.ty,
            place,
            scope: variable.scope,
        })
    }));
    if told.is_empty() && scopes.inlined.is_empty() {
        return Ok(None);
    }
    let inlined = inlined_ranges(scopes, lines, len, frame)?;
    let scopes = if told.is_empty() { Vec::new_in(frame) } else { ranges(scopes, lines, len, frame)? };
    let frame_base = Cow::Borrowed(&slots.frame_base[..]);
    let side = Written { version: VERSION, frame_base, scopes, variables: told, inlined };
    let size = postcard::experimental::serialized_size(&side).map_err(|e| e.to_string())?;
    // alloc: the side table's bytes, kept in the store: one allocation of their exact size
    let bytes = postcard::to_extend(&side, Vec::with_capacity(size)).map_err(|e| e.to_string())?;
    let id = ContentId::of(SIDE_DOMAIN, &bytes);
    Ok(Some((bytes, id)))
}
```

`postcard::experimental::serialized_size` and `postcard::to_extend(&T, W: Extend<u8>) ->
Result<W>` are in postcard 1.1.3 (`src/lib.rs:60`, `src/ser/mod.rs:269`, `:490`). The workspace
has its `alloc` feature.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `of` (168-176)

> applied by codegen-v2 (2026-10-29): as written.

```rust
pub(crate) fn of(
    variables: &[Variable],
    scopes: &Scopes,
    code: &crate::fragment::Code,
    frame: &Frame,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let len = u32::try_from(code.bytes.len()).map_err(|e| e.to_string())?;
    encode(variables, scopes, &code.slots, &code.lines, len, frame)
}
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `ranges` and `inlined_ranges` (180-219)

> applied by codegen-v2 (2026-10-29): as written, with `extend` over the frame's lists.

```rust
fn ranges<'f>(scopes: &Scopes, lines: &[LineRow], len: u32, frame: &'f Frame) -> Result<Vec<WrittenScope<'f>, &'f Frame>, String> {
    let mut out: Vec<WrittenScope<'f>, &'f Frame> = Vec::with_capacity_in(scopes.parents.len(), frame);
    out.extend(scopes.parents.iter().map(|&parent| WrittenScope { parent, ranges: Vec::new_in(frame) }));
    if out.is_empty() {
        return Ok(out);
    }
    ... // unchanged
}

fn inlined_ranges<'f>(scopes: &Scopes, lines: &[LineRow], len: u32, frame: &'f Frame) -> Result<Vec<WrittenInlined<'f>, &'f Frame>, String> {
    let mut out: Vec<WrittenInlined<'f>, &'f Frame> = Vec::with_capacity_in(scopes.inlined.len(), frame);
    out.extend(scopes.inlined.iter().map(|it| WrittenInlined {
        parent: it.parent,
        call_site: it.call_site,
        ranges: Vec::new_in(frame),
    }));
    if out.is_empty() {
        return Ok(out);
    }
    ... // unchanged
}
```

`extend` (222-) takes the frame's lists:

```rust
fn extend<'f, T>(
    out: &mut [T],
    parts: impl Fn(&mut T) -> (Option<u32>, &mut Vec<(u32, u32), &'f Frame>),
    scope: Option<u32>,
    (from, to): (u32, u32),
) -> Result<(), String> {
```

The body is unchanged.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### Its tests (`side.rs`'s `mod tests`)

> applied by codegen-v2 (2026-10-29): the four tests in tasks and the new `a_table_is_postcard_of_its_side_and_allocates_only_its_bytes`, as written.

- `a_side_table_reads_back`:

  ```rust
        let encoded = || encode(&variables, &Scopes::default(), &slots, &[], 16);
  ```

  becomes:

  ```rust
        let encoded = || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                encode(&variables, &Scopes::default(), &slots, &[], 16, frame)
            })
        };
  ```

- `nothing_to_tell_is_no_table`:

  ```rust
        let none = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            encode(&[], &Scopes::default(), &SlotPlaces::default(), &[], 0, frame)
        });
        assert_eq!(none, Ok(None));
  ```

- `a_scope_is_the_code_of_its_rows`, from `let ranges = ..` to the end:

  ```rust
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let ranges = ranges(&scopes, &lines, 20, frame).expect("the ranges");
            assert_eq!((ranges[0].parent, &ranges[0].ranges[..]), (None, &[(4, 16)][..]));
            assert_eq!((ranges[1].parent, &ranges[1].ranges[..]), (Some(0), &[(8, 16)][..]));
            let unknown = [row(0, 9)];
            assert!(super::ranges(&scopes, &unknown, 4, frame).is_err());
        });
  ```

- `inlined_code_is_told_with_its_call_site`, from `let (bytes, _) = ..` to the end:

  ```rust
        let (bytes, _) = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            encode(&[], &scopes, &SlotPlaces::default(), &lines, 20, frame)
        })
        .expect("encoded")
        .expect("a table of the inlined code");
        let side = decode(&bytes).expect("decoded");
        assert!(side.variables.is_empty() && side.scopes.is_empty());
        assert_eq!(side.inlined, [
            Inlined { parent: None, call_site: site(1), ranges: vec![(4, 16)] },
            Inlined { parent: Some(0), call_site: site(2), ranges: vec![(8, 12)] },
        ]);
        let unknown = [row(0, 9)];
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            assert!(inlined_ranges(&scopes, &unknown, 4, frame).is_err());
        });
  ```

- New test:

  ```rust
    /// What `encode` writes is, byte for byte, what postcard writes of the `Side` it reads back
    /// as: writing from the frame changes no byte of the format. A warm encode allocates only
    /// the bytes it returns.
    #[test]
    fn a_table_is_postcard_of_its_side_and_allocates_only_its_bytes() {
        let slots = SlotPlaces {
            frame_base: vec![0x56],
            slots: vec![(0, VariablePlace::Frame(-16)), (2, VariablePlace::Ranges(vec![(0, 8, vec![0x50])]))],
        };
        let ty = ContentId::of("test", b"u32");
        let variable = |name: &str, slot, scope| Variable {
            name: Symbol::intern(name),
            param: false,
            ty,
            slot,
            pointer: None,
            scope,
        };
        let variables = [variable("a", 0, None), variable("b", 2, Some(1))];
        let scopes = Scopes {
            parents: vec![None, Some(0)],
            of_span: vec![SpanScope::Outside, SpanScope::In(0), SpanScope::In(1)],
            ..Scopes::default()
        };
        let row = |offset, span| LineRow { offset, span };
        let lines = [row(0, 0), row(4, 1), row(8, 2), row(12, 0)];
        let encoded = || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                encode(&variables, &scopes, &slots, &lines, 16, frame)
            })
        };
        let (bytes, id) = encoded().expect("encoded").expect("a table");
        let side = decode(&bytes).expect("decoded");
        assert_eq!(postcard::to_allocvec(&side).expect("postcard"), bytes);
        assert_eq!(side.scopes, [
            Scope { parent: None, ranges: vec![(4, 12)] },
            Scope { parent: Some(0), ranges: vec![(8, 12)] },
        ]);
        assert_eq!(side.variables[1].place, Place::Ranges(vec![(0, 8, vec![0x50])]));
        let (warm, warm_id) = blaze_alloc_count::assert_at_most(1, "a warm side table: its bytes", encoded)
            .expect("encoded")
            .expect("a table");
        assert_eq!((warm, warm_id), (bytes, id));
    }
  ```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.11 `v2/release.rs` and `v2/statics.rs`

> applied by codegen-v2 (2026-10-29): both, with the fix below: `emitted` takes `code: fragment::Code` by value and the function's `places`, `spans`, `variables` and `scopes` in place of `&Function`, so the assembly path moves its code out of `function.code` and nothing is copied.

`v2/release.rs` `emitted` (270-293, under `feature = "llvm"`), before:

```rust
    let side = super::side::of(&function.variables, &function.scopes, code)
        .map_err(|e| failed(unit, e))?;
    let (converted, extra) = super::places::resolve_owning(&function.places, refs, |resolve| {
        super::convert::function(
            unit.instance.key(),
            ...
            side.as_ref().map(|it| it.1),
        )
    });
    let converted = converted.map_err(|e| failed(unit, e))?;
    Ok(Emitted { function: converted, side: side.map(|it| it.0), extra })
```

After:

```rust
    let (converted, extra) = super::places::resolve_owning(&function.places, refs, |resolve| {
        let side = super::side::of(&function.variables, &function.scopes, &code, resolve.frame())?;
        let converted = super::convert::function(
            resolve.frame(),
            unit.instance.key(),
            ...
            side.as_ref().map(|it| it.1),
        )?;
        Ok::<_, String>((converted, side))
    });
    let (converted, side) = converted.map_err(|e| failed(unit, e))?;
    Ok(Emitted { function: converted, side: side.map(|it| it.0), extra })
```

Errors are the same, in the same order. One thing found while reading, not part of this change:
`emitted` takes `code: &crate::fragment::Code` and passes it to `convert::function`, whose
parameter is `code: fragment::Code` (by value, "Its bytes move into the result"). That doesn't
compile under `--features llvm`. With this change `emitted` should take `code: fragment::Code`
by value (`side::of` borrows it first, then `convert` takes it), and its caller should hand over
the code it made.

`v2/statics.rs:105-107`, before:

```rust
    let (data, mut relocs) = resolve(&value.places, refs, |resolve| {
        super::convert::value(arch, &value.blob, &value.requests, resolve)
    })?;
```

After:

```rust
    let (data, mut relocs) = resolve(&value.places, refs, |resolve| {
        super::convert::value(resolve.frame(), arch, &value.blob, &value.requests, resolve)
    })?;
```

`v2/statics.rs:69` (`super::shape::value(..)`) is unchanged; `value` opens its own task (1.9).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.12 Cranelift's `Context`: already one reused per worker

> applied by codegen-v1 (2026-10-29): nothing to add, as it says.

Nothing to add. `spare.rs` keeps up to `KEPT = 4` `cranelift_codegen::Context`s and
`FunctionBuilderContext`s per thread:

- `context()` takes one;
- `keep_context()` calls `context.clear()` and drops a context of more than `SINGLE_PASS`
  instructions;
- `builder()` / `keep_builder()` do the same for builder contexts;
- `with_context(func, work)` runs `work` and keeps the context.

`build()` takes its context and builder there (`mod.rs:5070`, `:5076`, `:5126`). v2's `compiled()`
compiles in `crate::with_context` (`v2/backend.rs:703`), which ends in `keep_context`, so its
`ctx.clear()` runs per worker. 1.5 adds the relocation and line-row lists to the same shelf.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.13 Tests (`hir-ty-codegen`)

> applied by codegen-v1 (2026-10-29): the `frame_map.rs`, `lent.rs` and `spare.rs` tests are in.

> applied by codegen-v2 (2026-10-29), in part: the 1.6 test (in `v2/places.rs`) and the 1.7 test (bound 10, see 1.7) by codegen-v2-mem-v2a; the `v2/tests.rs`, `frame_map.rs`, `lent.rs`, `spare.rs` and `side.rs` tests are their owners' (codegen-v1, codegen-v2-mem-v2b).

> applied by codegen-v2 (2026-10-29), in part: `a_warm_shape_allocates_nothing` and `a_warm_prepare_takes_its_tables_from_its_frame` in `v2/tests.rs`, and the `side.rs` test (1.10), by codegen-v2-mem-v2b; `frame_map.rs`, `lent.rs` and `spare.rs` tests are codegen-v1's.

These come in addition to those given with their changes above:

- `frame_map.rs`, 1.2: `a_frame_map_answers_as_an_arena_map`, `a_warm_frame_map_allocates_nothing`
  (0 allocations);
- `lent.rs`, 1.3: `a_lent_value_in_a_frame_list_is_the_one_kept` (0);
- `spare.rs`, 1.5: `a_kept_list_of_rows_is_reused_whole` (0);
- `v2/tests.rs`, 1.6: `every_libcall_is_named_as_cranelift_names_it`;
- `v2/convert/tests.rs`, 1.7: `a_warm_conversion_allocates_only_what_it_keeps` (2, both kept);
- `v2/side.rs`, 1.10: `a_table_is_postcard_of_its_side_and_allocates_only_its_bytes` (1, kept).

In `v2/tests.rs` (after `a_shape_is_the_same_each_time`, line 271):

```rust
/// A warm unit's shape takes nothing of the global allocator: its bytes are the thread's
/// buffer, hashed as written, and its tables the frame's. The key is the one the prepare gave.
#[test]
fn a_warm_shape_allocates_nothing() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    let unit = unit(&spec);
    let (shape, prepared) = prepared(&frontend, &spec, &root);
    let super::Prepared::Cranelift(function) = &prepared else { panic!("a Cranelift unit") };
    let of = || blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| super::shape::of(&unit, function, frame));
    let cold = of().expect("a shape");
    let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's shape", of).expect("a shape");
    assert_eq!(cold, warm);
    assert_eq!(warm, shape);
}

/// Preparing an instance again takes fewer of the global allocator's blocks than the first
/// time, and its frame is used. The rest is Cranelift's IR, salsa's results and the prepared
/// function, which it keeps: no prepare can take none (1.14).
#[test]
fn a_warm_prepare_takes_its_tables_from_its_frame() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    let (_, cold) = blaze_alloc_count::measure(|| prepared(&frontend, &spec, &root));
    let before = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Codegen).allocated;
    let ((shape, _), warm) = blaze_alloc_count::measure(|| prepared(&frontend, &spec, &root));
    let after = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Codegen).allocated;
    assert!(warm.allocations < cold.allocations, "warm {warm:?}, cold {cold:?}");
    assert!(after > before, "the prepare's tables went to no frame");
    assert_eq!(shape, prepared(&frontend, &spec, &root).0);
}
```

How these read the counters:

- **Which tag counter.** `blaze_mem::tag::task_ended` adds each task's peak to its tag's
  `allocated` (`crates/blaze-mem/src/tag.rs:196-202`).
- **The frame check.** It only needs `after > before`. Other tests may run Codegen tasks at the
  same time, but the counter only grows, so the check can't fail wrongly; it can only miss.
- **Thread-local counts.** `blaze_alloc_count::measure` counts the calling thread.
- **The fixture.** `Used` derives `Debug` (`crates/blaze-alloc-count/src/lib.rs:30`), so the
  message prints it. `SIBLINGS`' `roots` has no debug info
  (`spec()` has `DebugInfo::None`), so `scoped` and `side` are not reached; the side table's own
  test covers them.

The existing shape tests (`siblings_over_pointers_have_one_shape`,
`different_constants_are_different_shapes`, `a_shape_is_the_same_each_time`) compare `Shape`s,
which are now `ContentId`s (2.2). Two shapes are equal exactly when their bytes were, given the
same unit, so they keep testing the same thing. `prepared`/`shape` in `v2/tests.rs` are
unchanged.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 1.14 What stays on the global heap in `hir-ty-codegen`, and why

> applied by codegen-v1 (2026-10-29): `// alloc:` notes on `lazy`, `slots`, the variant `Box<Layout>` and the shelves; the `symbol_of` note in `v2/frontend.rs` is codegen-v2's.

| what | where | why it stays |
| --- | --- | --- |
| Cranelift's IR (`ir::Function`, its `DataFlowGraph`, `Layout`, `SecondaryMap`s) | built in `build()`, moved into `Function` | **Third party, kept.** Cranelift's types take no allocator, and the IR is kept from prepare to emit. |
| Cranelift's compile (VCode, regalloc2, `MachBuffer`) | `ctx.compile` in `emit_with` | **Third party.** Most of it is kept warm by the reused `Context` (1.12); what grows past the thread's largest earlier function allocates inside Cranelift. |
| `compiled.buffer.data().to_vec()` | `emit_with` | **Kept.** The function's code, once, at its exact length. |
| `rustc_abi::Layout` copies (`Box<Layout>` in `KeptLayouts::variants`) | `variant_shape` | **Owns heap memory.** A `Layout` owns `Vec`s; in a frame its destructor would never run. Only the list of `Box`es is in the frame. |
| salsa results (`Arc<Layout>`, MIR, walked refs) | queries | **salsa's.** They are memoized, and borrowed here. |
| `forwarded: FxHashMap` | locals setup, `note_shareable` | **Iterated.** Its order (13323) decides `members`' order; to move it, sort it first. |
| `lazy`, `slots` | `note_shareable` | **Kept** in `cg.frame_locals` past the build. |
| `Function`'s fields (`places`, `spans`, `variables`, `scopes.parents`, `of_span`, `inlined`) | `prepared` | **Kept** by the driver until the emit. |
| `CompiledFunction`'s fields, `Emitted::side`, `Placed::own` (`extra`) | `convert`, `side`, `resolve_owning` | **Kept**, allocated once at their exact size. |
| `symbol_of` (`Option<String>`) | `v2/frontend.rs:521`, from `foldable` and `unsanitized` | **Rare.** Only a function with a symbol name (`#[no_mangle]`, `#[export_name]`, linked items, the C `main`, the allocator shim) allocates; `foldable` only asks `is_some()`. Add `// alloc: a symbol name, only for an item that has one` at 309 and 298. |
| `libcall_symbol` | `libcall_ref` (the walk) | **Kept** in the walk's `FrontRef::Extern`. |
| LLVM's allocations | `v2/release.rs` | **Third party** (C++); the conversion after it is on the frame (1.11). |
| `wasm.rs` `lower` | `emit_wasm` | **Not v2.** v1's wasm path only; v2 refuses wasm units. |
| Error text (`format!`, `to_string`) | every `Err` path | **Error paths.** Their notes are in 2.7. |

---

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

## 2. `crates/blaze-codegen`

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.1 `Cargo.toml` and `lib.rs`

`crates/blaze-codegen/Cargo.toml`, `[dependencies]` (between `blaze-items` and `blaze-query`):

```toml
blaze-mem.workspace = true
```

and a new section:

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
```

`crates/blaze-codegen/src/lib.rs`, after the `//!` header and before the first item:

```rust
#![feature(allocator_api)]
```

and before `pub mod backend;` (line 20):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> applied by codegen-v2 (2026-10-29): `blaze-mem` and the `blaze-alloc-count` dev-dependency are in `Cargo.toml`; `#![feature(allocator_api)]` and the counting allocator are in `lib.rs`, which also re-exports `RelocsView` (requested of `relocs.rs`, see 2.3).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.2 `backend.rs`

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Shape` (106-121): the key, not the bytes

The driver only ever asks a shape for its key (`driver.rs:777`). The bytes, which are often
larger than the IR, were kept in the `prepared` map for every pending unit until its emit. Now the
front end hashes them as it writes them (1.9) into its scratch buffer.

Before:

```rust
/// The canonical bytes of a prepared function: its IR with every reference by its place in
/// [`Described::refs`] and every setting that changes its code. Two with equal bytes compile
/// to one [`crate::template::Template`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Shape(pub Vec<u8>);

pub const SHAPE_DOMAIN: &str = "blaze v2 shape 1";

impl Shape {
    pub fn key(&self, unit: &Unit) -> ContentId {
        let mut hasher = blaze_ids::Hasher::new(SHAPE_DOMAIN);
        hasher.id(&unit.instance.settings.0);
        hasher.update(&[crate::content::arch_tag(unit.arch)]).part(&self.0);
        hasher.finish()
    }
}
```

After:

```rust
/// What a prepared function compiles to, up to what fills its places: the content ID of its
/// canonical bytes (its IR with every reference by its place in [`Described::refs`] and every
/// setting that changes its code), its unit's settings and architecture. Two with one compile
/// to one [`crate::template::Template`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Shape(ContentId);

pub const SHAPE_DOMAIN: &str = "blaze v2 shape 1";

impl Shape {
    /// The shape of `bytes`, the canonical bytes of a prepared function of `unit`: hashed as
    /// given, kept nowhere.
    pub fn of(unit: &Unit, bytes: &[u8]) -> Shape {
        let mut hasher = blaze_ids::Hasher::new(SHAPE_DOMAIN);
        hasher.id(&unit.instance.settings.0);
        hasher.update(&[crate::content::arch_tag(unit.arch)]).part(bytes);
        Shape(hasher.finish())
    }

    pub fn key(&self) -> ContentId {
        self.0
    }
}
```

The hash is the one `key` made, so every memo key and store key is unchanged (`SHAPE_DOMAIN`
stays "1"). Callers:

- `driver.rs:777`: `let shape = shape.key(&node.unit);` becomes `let shape = shape.key();`.
- `driver/tests.rs:141-151`: `_: &Unit` becomes `unit: &Unit`, and
  `Ok((Shape(code(&prepared)), prepared))` becomes
  `Ok((Shape::of(unit, &code(&prepared)), prepared))`.
- `crates/hir-ty-codegen/v2/shape.rs`: see 1.9.

`rg -n 'Shape\(' crates --glob '*.rs'` lists no other constructor or field use. `Shape` is now
`Copy` and 32 bytes, so the `prepared` maps (`HashMap<InstanceKey, (Shape, B::Prepared)>`) hold
no heap bytes for it.

> applied by codegen-v2 (2026-10-29): `Shape(ContentId)` with `Shape::of(unit, bytes)` and `key()` is in `backend.rs`; the `driver.rs`/`driver/tests.rs` callers are codegen-v2-mem-blob's, and `v2/shape.rs` already calls `Shape::of`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `release_refs` (157-169)

```rust
    let mut seen: std::collections::HashSet<ContentId> =
        own.iter().map(crate::content::target_id).collect();
    let mut refs = own.to_vec();
```

becomes:

```rust
    let mut seen: rustc_hash::FxHashSet<ContentId> =
        rustc_hash::FxHashSet::with_capacity_and_hasher(own.len(), Default::default());
    seen.extend(own.iter().map(crate::content::target_id));
    // alloc: the release unit's references, kept by its job
    let mut refs = own.to_vec();
```

`seen` is only asked `insert`, never iterated, so the hasher changes nothing seen outside. A
release unit's compile is LLVM's, which dwarfs this; the set is sized up front and hashes with Fx,
as the rest of the crate does.

> applied by codegen-v2 (2026-10-29): `release_refs` uses a presized `FxHashSet`, with its `// alloc:` and `// clone:` notes.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.3 `template.rs`: one copy from template to function

What the code did before:

- **Instantiate.** `Template::instantiate` cloned every part of the template into the function.
- **A stored template.** It was first decoded into a `Template` (every part copied out of the
  store's bytes) and then instantiated (copied again).
- **A compile's template.** `Template::of` cloned the function a compile had just given, and the
  driver then dropped that function.
- **Every list.** Each was built by `.collect::<Result<Vec<_>, _>>()`, which grows by doubling,
  because `Result`'s collect gives no lower bound.

After:

- `TemplateView` borrows a template, or a stored blob, with its small lists in a frame.
- An instance's function is made from a view with each part copied once, at its exact size.
- `Template::of_owned` takes a compile's function over by moving its parts.

Imports: `use blaze_mem::{Frame, Tag};` and, in `crate::{..}`, `Covered`, which is already
imported (`Covered::ARRAYS` is used).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### New types, after `TemplateUnwind` (101)

```rust
/// A [`Template`] borrowed: from a template ([`Template::view`]) or straight from a stored blob
/// ([`crate::blob::decode_view`]), its lists in a frame. An instance's function is made of one
/// with each part copied once, into the function ([`TemplateView::instantiate`]).
pub struct TemplateView<'a> {
    pub arch: Arch,
    pub align: u32,
    pub code: &'a [u8],
    pub relocs: &'a [TemplateReloc],
    pub data: &'a [DataView<'a>],
    pub inlined: &'a [CodeHash],
    pub side: Option<ContentId>,
    pub unwind: Option<UnwindView<'a>>,
    pub lines: &'a [Line],
    pub refs: u32,
    pub externs: &'a [&'a str],
    /// Owned: the function made of the view takes it as it is.
    pub coverage: Option<Covered>,
}

/// A [`TemplateData`] borrowed.
#[derive(Clone, Copy)]
pub struct DataView<'a> {
    pub align: u32,
    pub bytes: &'a [u8],
    pub relocs: &'a [TemplateReloc],
}

/// A [`TemplateUnwind`] borrowed.
#[derive(Clone, Copy)]
pub enum UnwindView<'a> {
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: &'a [u8],
        fde: &'a [u8],
        personality: Option<u32>,
        lsda: Option<u32>,
    },
    Windows { info: &'a [u8], handler: Option<u32>, lsda: Option<u32> },
}

impl TemplateData {
    pub fn view(&self) -> DataView<'_> {
        DataView { align: self.align, bytes: &self.bytes, relocs: &self.relocs }
    }
}

impl DataView<'_> {
    fn owned(self) -> TemplateData {
        TemplateData { align: self.align, bytes: self.bytes.to_vec(), relocs: self.relocs.to_vec() }
    }
}

impl TemplateUnwind {
    pub fn view(&self) -> UnwindView<'_> {
        match self {
            TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => {
                UnwindView::Dwarf {
                    code_align: *code_align,
                    data_align: *data_align,
                    return_address: *return_address,
                    cie,
                    fde,
                    personality: *personality,
                    lsda: *lsda,
                }
            }
            TemplateUnwind::Windows { info, handler, lsda } => {
                UnwindView::Windows { info, handler: *handler, lsda: *lsda }
            }
        }
    }
}

impl UnwindView<'_> {
    fn owned(self) -> TemplateUnwind {
        match self {
            UnwindView::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => {
                TemplateUnwind::Dwarf {
                    code_align,
                    data_align,
                    return_address,
                    cie: cie.to_vec(),
                    fde: fde.to_vec(),
                    personality,
                    lsda,
                }
            }
            UnwindView::Windows { info, handler, lsda } => {
                TemplateUnwind::Windows { info: info.to_vec(), handler, lsda }
            }
        }
    }
}

impl TemplateView<'_> {
    /// The function of instance `key` whose references are `refs`: [`Template::instantiate`]
    /// of the template the view is of, each part copied once.
    pub fn instantiate(self, key: InstanceKey, refs: &Refs) -> Result<CompiledFunction, TemplateError> {
        check_refs(refs)?;
        if refs_len(refs) != self.refs {
            return Err(TemplateError::Refs { made: self.refs, given: refs_len(refs) });
        }
        let data = instantiate_data(self.data, refs, self.externs)?;
        Ok(CompiledFunction {
            key,
            arch: self.arch,
            align: self.align,
            // alloc: the instance's code, kept by the build
            code: self.code.to_vec(),
            relocs: instantiate_relocs(self.relocs, refs, self.externs, &data, None)?,
            unwind: self.unwind.map(|it| instantiate_unwind(it, refs, &data)).transpose()?,
            data,
            // alloc: kept by the build
            inlined: self.inlined.to_vec(),
            side: self.side,
            // alloc: kept by the build
            lines: self.lines.to_vec(),
            coverage: self.coverage,
        })
    }

    /// The template the view is of, every part copied.
    pub fn owned(self) -> Template {
        Template {
            arch: self.arch,
            align: self.align,
            code: self.code.to_vec(),
            relocs: self.relocs.to_vec(),
            // alloc: a template its caller keeps, each list at its exact length
            data: self.data.iter().map(|it| it.owned()).collect(),
            inlined: self.inlined.to_vec(),
            side: self.side,
            unwind: self.unwind.map(UnwindView::owned),
            lines: self.lines.to_vec(),
            refs: self.refs,
            externs: self.externs.iter().map(|it| (*it).to_owned()).collect(),
            coverage: self.coverage,
        }
    }
}
```

`instantiate` builds its fields in the order the old `Template::instantiate` did (data, code,
relocations, unwind information), so with several errors the first one reported is the same.

> applied by codegen-v2 (2026-10-29): the three view types and their `view`/`owned`/`instantiate` are in `template.rs`, adapted for the `Relocs` table: `relocs: &'a [TemplateReloc]` is `relocs: RelocsView<'a, Ref>` (a borrowed table, requested of `relocs.rs` in `~/restart-2026-10-28/briefs/v2/codegen-v2-mem-template.requests.md`), and `TemplateView::instantiate` opens a `frame::task` for the tables' indexes.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `impl Template` (363-410)

`of` and `instantiate` become:

```rust
    /// `function` with each of its references by its place in `refs`. `extra` are the outside
    /// symbols its backend named of its own accord ([`crate::backend::Emitted::extra`]): a
    /// relocation to one of them that `refs` lacks is kept by name in the template.
    pub fn of(function: &CompiledFunction, refs: &Refs, extra: &[String]) -> Result<Template, TemplateError> {
        // clone: for a caller that keeps the function; a compile's goes to `of_owned`
        Template::of_owned(function.clone(), refs, extra)
    }

    /// [`Template::of`] of a function the caller has no more use for (a compile's): its code,
    /// data bytes, lines, unwind information and coverage move into the template.
    pub fn of_owned(function: CompiledFunction, refs: &Refs, extra: &[String]) -> Result<Template, TemplateError> {
        check_refs(refs)?;
        let externs = own_externs(refs, extra)?;
        let relocs = template_relocs(&function.relocs, refs, &externs, &function.data, None)?;
        let mut data = Vec::with_capacity(function.data.len());
        for (at, it) in function.data.iter().enumerate() {
            let relocs = template_relocs(&it.relocs, refs, &externs, &function.data, Some(at as u32))?;
            data.push(TemplateData { align: it.align, bytes: Vec::new(), relocs });
        }
        let unwind = function.unwind.map(|it| template_unwind(it, refs, &function.data)).transpose()?;
        for (made, it) in data.iter_mut().zip(function.data) {
            made.bytes = it.bytes;
        }
        Ok(Template {
            arch: function.arch,
            align: function.align,
            code: function.code,
            relocs,
            data,
            inlined: function.inlined,
            side: function.side,
            unwind,
            lines: function.lines,
            refs: refs_len(refs),
            externs,
            coverage: function.coverage,
        })
    }

    /// The template borrowed, its data and outside symbols listed in `frame`.
    pub fn view<'a>(&'a self, frame: &'a Frame) -> TemplateView<'a> {
        TemplateView {
            arch: self.arch,
            align: self.align,
            code: &self.code,
            relocs: &self.relocs,
            data: frame.alloc_slice_fill_with(self.data.len(), |at| self.data[at].view()),
            inlined: &self.inlined,
            side: self.side,
            unwind: self.unwind.as_ref().map(TemplateUnwind::view),
            lines: &self.lines,
            refs: self.refs,
            externs: frame.alloc_slice_fill_with(self.externs.len(), |at| self.externs[at].as_str()),
            // clone: the function made of the view takes it
            coverage: self.coverage.clone(),
        }
    }

    /// The function of instance `key` whose references are `refs`.
    pub fn instantiate(&self, key: InstanceKey, refs: &Refs) -> Result<CompiledFunction, TemplateError> {
        frame::task(Tag::Codegen, |frame| self.view(frame).instantiate(key, refs))
    }
```

with `use blaze_mem::frame;` among the imports.

`of_owned` checks in the old order: references, outside symbols, relocations, data in order,
unwind information. The data's bytes move in only after every check has passed, so an error
returns before any part has moved. `function.unwind.map(..)` moves `unwind` and borrows
`function.data`; these are disjoint fields of a struct with no `Drop`.

`id` is unchanged.

> applied by codegen-v2 (2026-10-29): `of` goes through the new `of_owned`, and `view` and `instantiate` are as given; the relocation tables are mapped with `RelocsView::try_map_in` in a `frame::task`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### The helpers (218-357)

`template_relocs`, with the same `match` and an exact loop in place of the collect:

```rust
fn template_relocs(
    relocs: &[Reloc],
    refs: &Refs,
    externs: &[String],
    data: &[Data],
    before: Option<u32>,
) -> Result<Vec<TemplateReloc>, TemplateError> {
    let mut made = Vec::with_capacity(relocs.len());
    for reloc in relocs {
        let target = match &reloc.target {
            ... // the match of 229-247, unchanged
        };
        made.push(TemplateReloc { at: reloc.at, kind: reloc.kind, target, addend: reloc.addend });
    }
    Ok(made)
}
```

The `return Err(..)`s in the match returned from the closure, which ended the collect; now they
return from the function, with the same error.

`template_data` (used by `StaticTemplate::of`):

```rust
fn template_data(data: &[Data], refs: &Refs, externs: &[String]) -> Result<Vec<TemplateData>, TemplateError> {
    let mut made = Vec::with_capacity(data.len());
    for (at, it) in data.iter().enumerate() {
        made.push(TemplateData {
            align: it.align,
            // clone: a static's value, once a static
            bytes: it.bytes.clone(),
            relocs: template_relocs(&it.relocs, refs, externs, data, Some(at as u32))?,
        });
    }
    Ok(made)
}
```

`instantiate_data` and `instantiate_relocs` take views and any list of names:

```rust
/// The data of a template with `refs` in its places, each with its content ID.
fn instantiate_data<E: AsRef<str>>(
    data: &[DataView<'_>],
    refs: &Refs,
    externs: &[E],
) -> Result<Vec<Data>, TemplateError> {
    let mut made: Vec<Data> = Vec::with_capacity(data.len());
    for (at, it) in data.iter().enumerate() {
        let relocs = instantiate_relocs(it.relocs, refs, externs, &made, Some(at as u32))?;
        // alloc: the instance's data, kept by the build
        made.push(Data::new(it.align, it.bytes.to_vec(), relocs));
    }
    Ok(made)
}

fn instantiate_relocs<E: AsRef<str>>(
    relocs: &[TemplateReloc],
    refs: &Refs,
    externs: &[E],
    data: &[Data],
    before: Option<u32>,
) -> Result<Vec<Reloc>, TemplateError> {
    let mut made = Vec::with_capacity(relocs.len());
    for reloc in relocs {
        let target = match reloc.target {
            // clone: the instance's own target, kept by the build
            Ref::Place(at) => refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))?,
            Ref::Extern(at) => {
                let name = externs.get(at as usize).ok_or(TemplateError::Extern(at))?;
                // alloc: the instance's own target, kept by the build
                Target::Extern(name.as_ref().to_owned())
            }
            ... // Data, Position, Coverage arms unchanged
        };
        made.push(Reloc { at: reloc.at, kind: reloc.kind, target, addend: reloc.addend });
    }
    Ok(made)
}
```

`template_unwind` takes the function's unwind information by value. It is only used by
`of_owned`; `of` goes through `of_owned` too.

```rust
fn template_unwind(unwind: Unwind, refs: &Refs, data: &[Data]) -> Result<TemplateUnwind, TemplateError> {
    let routine = |it: &Option<Target>| it.as_ref().map(|it| place(refs, it)).transpose();
    let lsda = |it: &Option<ContentId>| it.map(|it| data_index(data, it)).transpose();
    Ok(match unwind {
        Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            TemplateUnwind::Dwarf {
                code_align,
                data_align,
                return_address,
                personality: routine(&personality)?,
                lsda: lsda(&area)?,
                cie,
                fde,
            }
        }
        Unwind::Windows { info, handler, lsda: area } => {
            TemplateUnwind::Windows { handler: routine(&handler)?, lsda: lsda(&area)?, info }
        }
    })
}
```

The old code cloned `cie`/`fde`/`info` before checking `personality`/`lsda`; the clones could not
fail, so the errors are the same.

> applied by codegen-v2 (2026-10-29): the helpers take views and a frame, map tables with `try_map_in` in place of the old `.collect()` loops, and `template_data` fills a presized `Vec`.

`instantiate_unwind` takes a view:

```rust
fn instantiate_unwind(unwind: UnwindView<'_>, refs: &Refs, data: &[Data]) -> Result<Unwind, TemplateError> {
    let routine = |it: Option<u32>| {
        // clone: the instance's own target, kept by the build
        it.map(|at| refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))).transpose()
    };
    let lsda =
        |it: Option<u32>| it.map(|at| data.get(at as usize).map(|it| it.id).ok_or(TemplateError::Place(at))).transpose();
    Ok(match unwind {
        UnwindView::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            Unwind::Dwarf {
                code_align,
                data_align,
                return_address,
                // alloc: kept by the build
                cie: cie.to_vec(),
                fde: fde.to_vec(),
                personality: routine(personality)?,
                lsda: lsda(area)?,
            }
        }
        UnwindView::Windows { info, handler, lsda: area } => {
            // alloc: kept by the build
            Unwind::Windows { info: info.to_vec(), handler: routine(handler)?, lsda: lsda(area)? }
        }
    })
}
```

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `StaticTemplate` (463-509)

`of` is unchanged; it uses the exact `template_data` and `template_relocs`. In `instantiate`,
before:

```rust
        let data = instantiate_data(&self.data, refs, &[])?;
        let init = match &self.init {
            TemplateInit::Bytes { bytes, relocs } => {
                Init::Bytes { bytes: bytes.clone(), relocs: instantiate_relocs(relocs, refs, &[], &data, None)? }
            }
```

after:

```rust
        let data = frame::task(Tag::Codegen, |frame| {
            let views = frame.alloc_slice_fill_with(self.data.len(), |at| self.data[at].view());
            instantiate_data(views, refs, NO_EXTERNS)
        })?;
        let init = match &self.init {
            TemplateInit::Bytes { bytes, relocs } => {
                // clone: a static's value, kept by the build
                Init::Bytes { bytes: bytes.clone(), relocs: instantiate_relocs(relocs, refs, NO_EXTERNS, &data, None)? }
            }
```

with, beside `TEMPLATE_DOMAIN`:

```rust
/// The outside symbols of a static's template: a static names none of its own accord.
const NO_EXTERNS: &[&str] = &[];
```

> applied by codegen-v2 (2026-10-29): `StaticTemplate::instantiate` is as given, with its `init` inside the same frame; `of` maps its tables in a `frame::task` too, with the same checks in the same order.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.4 `blob.rs`: a stored template read in place

Imports: `use blaze_mem::{Frame, Tag, frame};` and, from `crate::template`, `DataView`,
`TemplateView`, `UnwindView`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `In` (160-318)

Its readers of lists gain frame versions; the owned ones stay for `decode_static` and
`decode_masked`. Add:

```rust
    /// `len`-counted bytes, borrowed from the blob.
    fn slice(&mut self) -> Result<&'a [u8], BlobError> {
        let len = self.count(1)?;
        self.take(len)
    }

    fn reloc(&mut self) -> Result<TemplateReloc, BlobError> {
        let at = self.u32()?;
        let Some(kind) = kind_of(self.u8()?) else { return self.fail("no such relocation kind") };
        let addend = self.i64()?;
        let target = match self.u8()? {
            ... // the match of 221-231, unchanged
        };
        Ok(TemplateReloc { at, kind, target, addend })
    }

    fn relocs_in(&mut self, frame: &'a Frame) -> Result<&'a [TemplateReloc], BlobError> {
        let count = self.count(RELOC_BYTES)?;
        let mut relocs = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            relocs.push(self.reloc()?);
        }
        Ok(relocs.leak())
    }

    fn data_in(&mut self, frame: &'a Frame) -> Result<&'a [DataView<'a>], BlobError> {
        let count = self.count(DATA_BYTES)?;
        let mut data = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            let align = self.u32()?;
            let bytes = self.slice()?;
            let relocs = self.relocs_in(frame)?;
            data.push(DataView { align, bytes, relocs });
        }
        Ok(data.leak())
    }

    fn unwind_view(&mut self) -> Result<Option<UnwindView<'a>>, BlobError> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(UnwindView::Dwarf {
                code_align: self.u32()?,
                data_align: i32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")),
                return_address: u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")),
                cie: self.slice()?,
                fde: self.slice()?,
                personality: self.index()?,
                lsda: self.index()?,
            }),
            2 => Some(UnwindView::Windows { info: self.slice()?, handler: self.index()?, lsda: self.index()? }),
            _ => return self.fail("no such unwind information"),
        })
    }

    fn lines_in(&mut self, frame: &'a Frame) -> Result<&'a [Line], BlobError> {
        let count = self.count(4 + 32 + 4)?;
        let mut lines = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            let at = self.u32()?;
            lines.push(Line { at, span: SpanRef { anchor: self.id()?, index: self.u32()? } });
        }
        Ok(lines.leak())
    }
```

Existing ones that change:

- `relocs` becomes the same loop over `self.reloc()?`, pushing into its exact
  `Vec::with_capacity(count)`.
- `bytes()` stays: `TemplateInit::Bytes` and `decode_masked` own theirs.
- `data` and `unwind` stay as they are for `decode_static` and `decode_masked`.

`Vec::leak` on a `Vec<T, &'a Frame>` gives `&'a mut [T]`. A reader that fails part way drops its
partial `Vec` back into the frame. As before, a count is checked against what is left before
anything is reserved, so a hostile length reserves nothing.

> applied by codegen-v2 (2026-10-29): on `blaze_format::cursor`'s `Reader`, with `slice`, `relocs_in`, `data_in`, `unwind_view` and `lines_in` added and the row, target, wide-addend and line readers shared with the owned ones. Relocations are a table, so `relocs_in` reads the rows, targets and wide addends into the frame and checks them with mem-template's `RelocsView::from_parts(.., frame)` (requested of `relocs.rs`), with `from_parts`' words at the same byte. The owned `unwind` and `lines` go, since only the old `decode` used them.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `decode` (350-381)

Before:

```rust
pub fn decode(bytes: &[u8]) -> Result<Template, BlobError> {
    let mut input = In { bytes, at: 0 };
    ...
    let coverage = input.coverage()?;
    input.end()?;
    Ok(Template { arch, align, code, relocs, data, inlined, side, unwind, lines, refs, externs, coverage })
}
```

After:

```rust
/// The template in `bytes`, borrowed from them, its lists in `frame`: what instantiating a
/// stored template reads, copying each part once, into the function.
pub fn decode_view<'a>(bytes: &'a [u8], frame: &'a Frame) -> Result<TemplateView<'a>, BlobError> {
    let mut input = In { bytes, at: 0 };
    input.magic(TEMPLATE_MAGIC)?;
    let Some(arch) = arch_of(input.u8()?) else { return input.fail("no such architecture") };
    let align = input.u32()?;
    if !align.is_power_of_two() {
        return input.fail("an alignment that is not a power of two");
    }
    let code = input.slice()?;
    let relocs = input.relocs_in(frame)?;
    let data = input.data_in(frame)?;
    let count = input.count(32)?;
    let mut inlined = Vec::with_capacity_in(count, frame);
    for _ in 0..count {
        inlined.push(CodeHash(input.id()?));
    }
    let side = match input.bool()? {
        true => Some(input.id()?),
        false => None,
    };
    let unwind = input.unwind_view()?;
    let lines = input.lines_in(frame)?;
    let refs = input.u32()?;
    let count = input.count(4)?;
    let mut externs = Vec::with_capacity_in(count, frame);
    for _ in 0..count {
        let name = input.slice()?;
        match std::str::from_utf8(name) {
            Ok(name) => externs.push(name),
            Err(_) => return input.fail("a symbol name that is not UTF-8"),
        }
    }
    // alloc: its PC table, which the function made of the view takes as it is
    let coverage = input.coverage()?;
    input.end()?;
    Ok(TemplateView {
        arch,
        align,
        code,
        relocs,
        data,
        inlined: inlined.leak(),
        side,
        unwind,
        lines,
        refs,
        externs: externs.leak(),
        coverage,
    })
}

/// The template in `bytes`.
pub fn decode(bytes: &[u8]) -> Result<Template, BlobError> {
    frame::task(Tag::Codegen, |frame| decode_view(bytes, frame).map(TemplateView::owned))
}
```

Every check, its order and its `BlobError { at, what }` are the old `decode`'s. A name that is not
UTF-8 fails at the byte after it, as `String::from_utf8` of `input.bytes()?` did. `decode` gives
the `Template` it gave.

> applied by codegen-v2 (2026-10-29): as written, on the cursor's `Reader` (`input.0.u8()` and the rest), with `relocs: RelocsView<'a, Ref>`. `decode` is `decode_view` and then `TemplateView::owned`, inside a `Codegen` task.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `encode` (320-348): one allocation of the exact size

```rust
    let mut out = Out(Vec::with_capacity(64 + template.code.len()));
```

becomes:

```rust
    // alloc: the blob, kept in the store: one allocation of its exact size
    let mut out = Out(Vec::with_capacity(encoded_len(template)));
```

with, after `encode`:

```rust
/// How many bytes [`encode`] writes of `template`.
fn encoded_len(template: &Template) -> usize {
    let bytes = |it: &[u8]| 4 + it.len();
    let relocs = |relocs: &[TemplateReloc]| {
        4 + relocs
            .iter()
            .map(|it| 4 + 1 + 8 + 1 + if matches!(it.target, Ref::Position(_)) { 32 + 4 } else { 4 })
            .sum::<usize>()
    };
    let index = |it: &Option<u32>| if it.is_some() { 5 } else { 1 };
    let unwind = match &template.unwind {
        None => 1,
        Some(TemplateUnwind::Dwarf { cie, fde, personality, lsda, .. }) => {
            1 + 4 + 4 + 2 + bytes(cie) + bytes(fde) + index(personality) + index(lsda)
        }
        Some(TemplateUnwind::Windows { info, handler, lsda }) => 1 + bytes(info) + index(handler) + index(lsda),
    };
    let data = 4 + template.data.iter().map(|it| 4 + bytes(&it.bytes) + relocs(&it.relocs)).sum::<usize>();
    let externs = 4 + template.externs.iter().map(|it| bytes(it.as_bytes())).sum::<usize>();
    let coverage = match &template.coverage {
        None => 1,
        Some(covered) => 1 + 3 * 4 + 4 + covered.pcs.len() * (4 + 8),
    };
    TEMPLATE_MAGIC.len()
        + 1
        + 4
        + bytes(&template.code)
        + relocs(&template.relocs)
        + data
        + 4
        + 32 * template.inlined.len()
        + 1
        + if template.side.is_some() { 32 } else { 0 }
        + unwind
        + 4
        + (4 + 32 + 4) * template.lines.len()
        + 4
        + externs
        + coverage
}
```

Each term is the matching `Out` call of `encode`:

| term | what `Out` writes |
| --- | --- |
| magic | 4 bytes |
| `arch_tag` | `u8` |
| `align` | `u32` |
| `bytes` | `len` `u32` and the bytes |
| a relocation | `at` `u32`, kind `u8`, `addend` `i64`, tag `u8`, then `u32`, or 32 + 4 for a `Position` |
| a data object | `align` `u32`, `bytes`, `relocs` |
| inlined | `len` and 32 a hash |
| side | `u8`, plus 32 |
| unwind | tag, `u32`, `i32`, `u16`, two `bytes`, two `index` (`u8` plus `u32` when `Some`) |
| a line | `u32`, 32-byte `id`, `u32` |
| refs | `u32` |
| externs | `len` and `bytes` each |
| coverage | `u8`, three `u32`, `len`, 12 bytes per PC |

> applied by codegen-v2 (2026-10-29): `encode` is `Vec::with_capacity(encoded_len(template))`, with a `debug_assert_eq!` of the length it wrote. A relocation term is the table's: `len` and 16 bytes a row, `len` and 5 bytes a target (37 for a `Position`), `len` and 12 bytes a wide addend.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.5 `driver.rs`: store hits read in place, compiles taken over

Imports: `use blaze_mem::{Tag, frame};` and `template::TemplateError` in the `crate::template`
import (line 45).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Lookup` (48-62)

Before:

```rust
/// The store as a build reads it.
pub trait Lookup {
    /// The verified bytes under `key`; `Err` names why what is there can't be read.
    fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, String>;
}

impl Lookup for blaze_cas::Store {
    fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, String> {
        match blaze_cas::Store::get(self, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes.to_vec())),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}
```

After:

```rust
/// The store as a build reads it.
pub trait Lookup {
    /// The verified bytes under `key`, borrowed from the store, read in place; `Err` names
    /// why what is there can't be read.
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String>;
}

impl Lookup for blaze_cas::Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        match blaze_cas::Store::get(self, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}
```

`blaze_cas::Store::get(&self, key) -> Result<Option<Entry<'_>>, Corrupt>`
(`crates/blaze-cas/src/lib.rs:867`) ties the entry's `bytes: &[u8]` (220) to `&self`, which is
what `read` returns them under.

`read` takes no callback, so the trait adds no `dyn`. The build's `&dyn Lookup`, chosen once
per build, is unchanged; its call is one virtual call per read, as `get` was.

`blaze-cargo`'s implementor changes with it (0.4).

> applied by codegen-v2 (2026-10-29): as written. `blaze-cargo`'s `Reading` still has `get` in the tree, though 0.4 is marked DONE; it is requested of cargo-units (told by tmux).

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Out::read` (309-330)

After (316-317 change; the rest is as it is):

```rust
    fn read<T>(
        &mut self,
        // dyn: the build's store, chosen once a build
        store: &dyn Lookup,
        key: &Key,
        unit: &Unit,
        decode: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Read<T> {
        match store.read(key) {
            Ok(Some(bytes)) => match decode(bytes) {
                Ok(it) => Read::Hit(it),
                Err(why) => {
                    // alloc: an error
                    self.errors.push(error(unit.instance.key(), unit.item, format!("the store's {key:?}: {why}")));
                    Read::Failed
                }
            },
            Ok(None) => Read::Miss,
            Err(why) => {
                // alloc: an error
                self.errors.push(error(unit.instance.key(), unit.item, format!("reading the store's {key:?}: {why}")));
                Read::Failed
            }
        }
    }
```

The outcomes and messages are today's. `decode` gets the store's bytes in place rather than an
owned copy. `decode` stays a generic `impl FnOnce`, which is monomorphized, not a `dyn`.

> applied by codegen-v2 (2026-10-29): with `store: &impl Lookup` in place of `&dyn Lookup` (dedyn-v2 1 in `v2/requests/codegen-v2.md`, applied in the same change, for `read`, `run`, `tier_up`, `dev`, `folded`, `release` and `statics`), so the `// dyn:` note isn't needed.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `Out::side`, `Out::instantiate`, `Out::template_of` (295-349) and two new functions

```rust
    /// A compile's side table under its content ID: its bytes move to the put.
    fn side(&mut self, unit: &Unit, emitted: &mut Emitted) {
        match (emitted.side.take(), emitted.function.side) {
            (Some(bytes), Some(id)) => self.put(keys::side(&id), Kind::Debug, bytes),
            (None, None) => {}
            _ => self.errors.push(error(
                unit.instance.key(),
                unit.item,
                "the compile's side table and the function's record of it disagree",
            )),
        }
    }

    /// The function of `template` for `key` with `refs`, into the build's functions.
    fn instantiate(&mut self, unit: &Unit, key: InstanceKey, template: &Template, refs: &[Target]) {
        self.made(unit, key, template.instantiate(key, refs));
    }

    /// A function made for `key`, into the build's functions, or why it could not be made.
    fn made(&mut self, unit: &Unit, key: InstanceKey, made: Result<CompiledFunction, TemplateError>) {
        match made {
            Ok(function) => self.functions.push(function),
            // alloc: an error
            Err(why) => self.errors.push(error(key, unit.item, why.to_string())),
        }
    }

    /// A compile's template against `refs`, taking the compile's function over, or the error
    /// naming why it has none.
    fn template_of(&mut self, unit: &Unit, emitted: Emitted, refs: &[Target]) -> Option<Template> {
        let key = emitted.function.key;
        match Template::of_owned(emitted.function, refs, &emitted.extra) {
            Ok(it) => Some(it),
            Err(why) => {
                // alloc: an error
                self.errors.push(error(key, unit.item, why.to_string()));
                None
            }
        }
    }
```

and, outside `impl Out`:

```rust
/// The function of instance `key` with `refs` in a stored template blob, read in place: its
/// lists are decoded into a frame and each part copied once, into the function. The outer
/// `Err` is bytes that are no template; the inner is the template's own.
fn stored_function(
    bytes: &[u8],
    key: InstanceKey,
    refs: &[Target],
) -> Result<Result<CompiledFunction, TemplateError>, String> {
    frame::task(Tag::Codegen, |frame| {
        // alloc: an error
        let view = blob::decode_view(bytes, frame).map_err(|e| e.to_string())?;
        Ok(view.instantiate(key, refs))
    })
}
```

> applied by codegen-v2 (2026-10-29): as written. `Out::instantiate` now goes through `made`.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### Call sites

**`dev`, the store's templates (745-760).** Before:

```rust
            let read = out.read(store, &keys::template(&node.stored()), &node.unit, |it| blob::decode(it).map_err(|e| e.to_string()));
            match read {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    prepared.remove(key);
                    out.instantiate(&node.unit, *key, &template, &node.refs);
                }
```

After:

```rust
            let read = out.read(store, &keys::template(&node.stored()), &node.unit, |it| {
                stored_function(it, *key, &node.refs)
            });
            match read {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    prepared.remove(key);
                    out.made(&node.unit, *key, made);
                }
```

**`dev`, shapes (777).** `let shape = shape.key(&node.unit);` becomes `let shape = shape.key();`.

**`dev`, compiles (796-821).** `let emitted = match emitted {` becomes
`let mut emitted = match emitted {`. Then:

```rust
            out.side(&node.unit, &emitted);
            let Some(template) = out.template_of(&node.unit, &emitted, &node.refs) else { continue };
```

becomes:

```rust
            out.side(&node.unit, &mut emitted);
            let Some(template) = out.template_of(&node.unit, emitted, &node.refs) else { continue };
```

`unkept_inline(nodes, &emitted.function, ..)` (811) runs before, on the borrowed function.

**`folded`, merges (909-944).** `let merged = match merged {` becomes
`let mut merged = match merged {`. Then:

```rust
            out.side(&first.unit, &merged.body);
            if let Some(body) = out.template_of(&first.unit, &merged.body, &first.refs) {
                ...
            }
            for (member, thunk) in group.members.iter().zip(&merged.thunks) {
                let node = &nodes[member];
                if thunk.function.key != *member {
                    ...
                }
                out.side(&node.unit, thunk);
                let refs = thunk_refs(group, member);
                if let Some(template) = out.template_of(&node.unit, thunk, &refs) {
```

becomes:

```rust
            out.side(&first.unit, &mut merged.body);
            if let Some(body) = out.template_of(&first.unit, merged.body, &first.refs) {
                ...
            }
            for (member, mut thunk) in group.members.iter().zip(merged.thunks) {
                let node = &nodes[member];
                if thunk.function.key != *member {
                    ...
                }
                out.side(&node.unit, &mut thunk);
                let refs = thunk_refs(group, member);
                if let Some(template) = out.template_of(&node.unit, thunk, &refs) {
```

`Merged { body: Emitted, thunks: Vec<Emitted> }` (`fold.rs:139`) has no `Drop`, so its fields
move out one by one. `merged.thunks.len()` (922, 925) is read before.

**`folded`, stored bodies and thunks (855-873).** These read whole templates, which `dev` keeps
for a fold (`out.instantiate(&first.unit, key, &body, ..)`, then each thunk), so they keep
`blob::decode`. It now decodes through a frame and copies once, into the template.

**`release`, the store's blobs (1059-1063).** Before:

```rust
            match out.read(store, &blob_key, &job.unit, |it| blob::decode(it).map_err(|e| e.to_string())) {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    out.instantiate(&job.unit, key, &template, &refs);
                }
```

After:

```rust
            match out.read(store, &blob_key, &job.unit, |it| stored_function(it, key, &refs)) {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    out.made(&job.unit, key, made);
                }
```

The closure's borrow of `refs` ends when `read` returns, before `Read::Miss` moves `refs` into
`missing`.

**`release`, compiles (1110-1129).** `let emitted = match emitted {` becomes
`let mut emitted = match emitted {`. Then:

```rust
            out.side(&job.unit, &emitted);
            if let Some(template) = out.template_of(&job.unit, &emitted, &refs) {
```

becomes:

```rust
            out.side(&job.unit, &mut emitted);
            if let Some(template) = out.template_of(&job.unit, emitted, &refs) {
```

What a warm dev build does per unit after this:

- **Stored hit.** The store's bytes are borrowed, and the template's lists go into a frame. The
  function is allocated once, part by part, at exact sizes. No `Vec` of the blob, no `Template`.
- **Memo hit.** The template is viewed in a frame and the function copied once.
- **Compile.** The function's parts move into the template. The side bytes move to the put. The
  blob is encoded at its exact size.

> applied by codegen-v2 (2026-10-29): every call site as written: `dev`'s store reads, shapes and compiles, `folded`'s merges (the stored bodies and thunks keep `blob::decode`), and `release`'s store reads and compiles. `unkept_inline` also returns before building its set when the function builds nothing in.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.6 Tests (`blaze-codegen`)

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `driver/tests.rs`

The test store (264-271), before:

```rust
impl Lookup for Store {
    fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.get(key).cloned())
    }
}
```

after:

```rust
impl Lookup for Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }
}
```

The test prepare (141-151): `_: &Unit` becomes `unit: &Unit` and
`Ok((Shape(code(&prepared)), prepared))` becomes
`Ok((Shape::of(unit, &code(&prepared)), prepared))` (2.2).

New, in `driver/tests.rs`:

```rust
/// A read of the store hands its bytes to the decoder in place: a warm hit allocates nothing
/// the decoder does not.
#[test]
fn a_store_hit_is_read_in_place() {
    let key = crate::query::keys::template(&InstanceKey(ContentId([1; 32])));
    let mut store = Store(BTreeMap::new());
    store.0.insert(key, vec![7; 4096]);
    let unit = Unit {
        instance: crate::Instance {
            code: blaze_items::CodeHash(ContentId::default()),
            args: ContentId::default(),
            settings: crate::Settings(ContentId::default()),
        },
        backend: crate::Backend::Cranelift,
        arch: crate::Arch::X86_64,
        item: blaze_items::ItemKey(ContentId::default()),
    };
    let mut out = Out::default();
    let sum = |bytes: &[u8]| Ok::<u64, String>(bytes.iter().map(|&it| u64::from(it)).sum());
    assert!(matches!(out.read(&store, &key, &unit, sum), Read::Hit(28672)));
    let read = blaze_alloc_count::assert_at_most(0, "a warm store hit", || out.read(&store, &key, &unit, sum));
    assert!(matches!(read, Read::Hit(28672)));
    let missing = crate::query::keys::template(&InstanceKey(ContentId([2; 32])));
    assert!(matches!(out.read(&store, &missing, &unit, sum), Read::Miss));
    assert!(out.errors.is_empty());
}
```

What the test relies on:

- **Paths.** `driver/tests.rs` is a child of `driver`, so it reaches the private `Out`, `Read` and
  `Store`. `crate::query::keys::template` is the key function `dev` uses (`keys::template`).
- **Field paths.** `Unit`, `Instance` and `Settings(pub ContentId)` are `lib.rs`'s (lines 39,
  67, 84). If `driver/tests.rs` already has a `Unit` helper, use it.
- **Keys.** `Key` is `Copy` (`crates/blaze-cas/src/lib.rs:129`).
- **`Out::default()`.** `Out` derives `Default` (`driver.rs:277`).

> applied by codegen-v2 (2026-10-29): the test store, the prepare and `a_store_hit_is_read_in_place` as written, with `Backend::Cranelift { speed: false }` and the file's `item(0)`. A new `a_stored_function_is_the_decoded_templates` checks that `stored_function` gives what `blob::decode` and `instantiate` give for every stored template of a cold build, with the right number of references and with one too many.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `template.rs` tests

Beside `reserve`:

```rust
    /// The heap blocks a function owns: what making it must allocate, and no more.
    fn owned_blocks(function: &CompiledFunction) -> u64 {
        let list = |len: usize| u64::from(len != 0);
        let names = |relocs: &[Reloc]| {
            let named = relocs.iter().filter(|it| matches!(&it.target, Target::Extern(name) | Target::Weak(name) if !name.is_empty()));
            named.count() as u64
        };
        let routine = |it: &Option<Target>| u64::from(matches!(it, Some(Target::Extern(name) | Target::Weak(name)) if !name.is_empty()));
        let unwind = match &function.unwind {
            None => 0,
            Some(Unwind::Dwarf { cie, fde, personality, .. }) => list(cie.len()) + list(fde.len()) + routine(personality),
            Some(Unwind::Windows { info, handler, .. }) => list(info.len()) + routine(handler),
        };
        let data: u64 = function.data.iter().map(|it| list(it.bytes.len()) + list(it.relocs.len()) + names(&it.relocs)).sum();
        list(function.code.len())
            + list(function.relocs.len())
            + names(&function.relocs)
            + list(function.data.len())
            + data
            + list(function.inlined.len())
            + list(function.lines.len())
            + unwind
            + function.coverage.as_ref().map_or(0, |it| list(it.pcs.len()))
    }

    /// A warm instantiate allocates exactly the blocks the function owns: each part copied
    /// once, the template's views in the frame.
    #[test]
    fn a_warm_instantiate_allocates_only_what_the_function_keeps() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        let made = blaze_alloc_count::assert_at_most(owned_blocks(&function), "a warm instantiate", || {
            template.instantiate(function.key, &refs)
        })
        .unwrap();
        assert_eq!(made, function);
    }

    /// The template of a compile takes the function's parts over: it allocates only its
    /// relocation lists and its list of data.
    #[test]
    fn a_compiles_template_takes_its_parts_over() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let expected = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(Template::of_owned(function.clone(), &refs, &[]).unwrap(), expected);
        let lists = u64::from(!function.relocs.is_empty())
            + u64::from(!function.data.is_empty())
            + function.data.iter().filter(|it| !it.relocs.is_empty()).count() as u64;
        let given = function.clone();
        let made = blaze_alloc_count::assert_at_most(lists, "a compile's template: its lists", || {
            Template::of_owned(given, &refs, &[])
        })
        .unwrap();
        assert_eq!(made, expected);
        assert_eq!(made.instantiate(function.key, &refs).unwrap(), function);
    }
```

For `reserve`, `owned_blocks` is 8:

- code, the relocation list, `memcpy`'s name (`Target::Extern` through `Ref::Place(2)`) and the
  data list: 4;
- `inner`'s bytes, `outer`'s bytes and `outer`'s relocations: 3;
- the lines: 1.

`lists` is 3: the function's relocations, the data list and `outer`'s relocations.

> applied by codegen-v2 (2026-10-29): both tests are in `template.rs`, plus `a_view_owns_back_its_template`. Budgets count each table's rows, targets and wide addends, so `owned_blocks` is 10 and `lists` is 5, and the tests assert both numbers. The `driver/tests.rs` and `blob.rs` tests are codegen-v2-mem-blob's.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

#### `blob.rs` tests

```rust
    /// Reading a stored template in place allocates nothing on a warm thread but the coverage
    /// table the function takes as it is: its code, data bytes and names are the blob's, its
    /// lists the frame's. It is the template `decode` gives.
    #[test]
    fn a_warm_view_of_a_blob_allocates_only_its_coverage() {
        let template = template();
        let bytes = encode(&template);
        let read = || {
            frame::task(Tag::Codegen, |frame| {
                decode_view(&bytes, frame).map(|view| (view.code.len(), view.relocs.len(), view.data.len(), view.externs.len()))
            })
        };
        let cold = read().expect("a view");
        let coverage = u64::from(template.coverage.as_ref().is_some_and(|it| !it.pcs.is_empty()));
        let warm = blaze_alloc_count::assert_at_most(coverage, "a warm read of a stored template", read).expect("a view");
        assert_eq!(cold, warm);
        assert_eq!(decode(&bytes).expect("decoded"), template);
        let mut none = template;
        none.coverage = None;
        let bytes = encode(&none);
        let read = || frame::task(Tag::Codegen, |frame| decode_view(&bytes, frame).map(|view| view.code.len()));
        read().expect("a view");
        blaze_alloc_count::assert_at_most(0, "a warm read of a template with no coverage", read).expect("a view");
    }

    /// `encode` sizes its bytes exactly: one allocation, of `encoded_len`.
    #[test]
    fn encode_writes_into_one_allocation_of_its_length() {
        let mut template = template();
        for _ in 0..2 {
            let (bytes, used) = blaze_alloc_count::measure(|| encode(&template));
            assert_eq!(bytes.len(), encoded_len(&template));
            assert_eq!(used.allocations, 1);
            template.coverage = None;
        }
    }
```

What these rely on:

- **The fixture.** `template()` is the fixture of `blob.rs`'s tests (it sets `coverage: Some(..)`,
  line 553). If it has no unwind information, add one `TemplateUnwind::Dwarf` in a third
  iteration, so `encoded_len` is checked for it too.
- **Imports.** `use blaze_mem::{Tag, frame};` in the test module.

The existing tests stay as they are and keep checking that `decode` and `instantiate` give what
they gave: every `decode(..)`/`encode(..)` round trip, `unwinding_names_its_personality..` and
the coverage tests.

> applied by codegen-v2 (2026-10-29): both tests as written. The fixture has DWARF unwind information already, so `encode_writes_into_one_allocation_of_its_length` loops over DWARF, Windows and none, each with and without coverage, and checks each round trip. A new `a_short_field_fails_where_its_read_starts` checks `BlobError::at` by the cursor's rule: a cut `align` fails at byte 5, where its read starts, from `decode` and from `decode_view` alike.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

### 2.7 `// alloc:`, `// clone:`, `// dyn:` notes for the lines the hot lint flags

The lint's findings in these files today:

- `driver.rs`: 104
- `template.rs`: 27
- `blob.rs`: 4
- `backend.rs`: 4

Their state after the changes above:

- **Gone with the changes.**
  - `driver.rs` 57-58 (`to_vec`, `to_string` in `get`) and 298 (`bytes.clone()`);
  - `template.rs` 249, 262, 304 (`collect`), 258, 270, 323-330, 347-354, 373-382 and 401-408
    (clones);
  - `blob.rs` 211 (the `to_vec` in `bytes()`, now only read by statics and masks).
- **Noted in the code above.** Every new `to_vec`/`to_owned`/`clone` in 2.2-2.5 carries its note.
- **Error paths, still to note.** Add `// alloc: an error` above `driver.rs` 703, 736, 803, 812,
  833, 1116 and every other `format!`/`to_string` in an `Err` arm or `map_err`. Add
  `// clone: an error names the target` above `template.rs` 181, 189 and 210.
- **Statics and masks, still to note.** Add `// clone: a static's value, kept by the build` above
  `template.rs` 468, 480, 494 and 506. Add `// alloc: a symbol name, kept by its static` above
  `blob.rs` 428, and `// alloc: a fold's mask, kept by its group` above 444.
- **Per build, not per unit, still to note.** Each gets `// alloc: per build, sized by its units`:
  - the build's own lists and maps: `driver.rs` 700, 724, 742, 745, 761, 771, 773, 797, 903, 910,
    1051, 1055, 1073, 1086, 1100 and 1109;
  - the `dyn Lookup` parameters at 601, 661, 689, 847, 952 and 1134
    (`// dyn: the build's store, chosen once a build`).

  Moving those onto the build's request arena is lane `codegen`'s driver work, not a unit's
  memory.

> applied by codegen-v2 (2026-10-29): `template.rs` and `backend.rs` have their notes, and `hot_lint.py` finds nothing in them; `driver.rs` and `blob.rs` are codegen-v2-mem-blob's.

> applied by codegen-v2 (2026-10-29): `driver.rs` and `blob.rs` have every note, and `hot_lint.py` finds nothing in them. The `dyn Lookup` lines are gone, not noted (`&impl Lookup`, see `Out::read`). Clones in the walk (front-end handles, origins), the per-tier-up lists and the daemon's memo `Arc` are noted as per build, per tier-up or once a daemon.

---

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

## 3. Structs that gain a lifetime or an allocator parameter, and their callers

> applied by codegen-v1 (2026-10-29): the `hir-ty-codegen` rows other than `v2/` (with `KeptLayouts` lifetime-free, see 1.4) and `emit_with(ctx, store, speed, relocs, lines)`.

> applied by codegen-v2 (2026-10-29), in part: `Placer`, `Placed`, `convert::function(frame, ..)` and `convert::value(frame, ..)` and their callers in `v2/release.rs` and `v2/statics.rs` by codegen-v2-mem-v2a; the rest is the other owners'. Section 4 touches none of these files.

> applied by codegen-v2 (2026-10-29), in part: `v2/shape.rs`'s `Out<'b>` and `Data<'f>`, `v2/side.rs`'s `Written*`, and the new parameters of `side::of`/`encode`, `shape::of`, `named`, `scoped` and `colocate`, with every caller in `v2/backend.rs`, by codegen-v2-mem-v2b; no signature declared in `v2.rs` changes. Section 4 touches none of codegen-v2-mem-v2b's files either.

| struct | new parameter | callers that change |
| --- | --- | --- |
| `hir-ty-codegen` `FunctionCx<'cx, 'db>` | none new; gains `frame: &'cx Frame` | `build()` only (1.4). Every method keeps its signature. |
| `GivenAway` | `GivenAway<'f>` | `FunctionCx::given_away`; the three users in 1.4 |
| `KeptLayouts` | `KeptLayouts<'f>` | `build()`, `Layouts::new`, the test at 20011 |
| `Layouts<'cx, 'db>` | none new; `kept: &'cx KeptLayouts<'cx>`, frame maps | `Layouts::new(&kept, frame)` in `build()` |
| `Lent<P>` | `Lent<P, A: Allocator = Global>` | none: `Default` stays for `Global` |
| `Placer<'a>` | `Placer<'a, A: Allocator = Global>` | `prepared` (`listed_in(.., frame)`); `place_requests` is generic over `A`. Every other caller is unchanged. |
| `Placed<'a>` | none new; gains `frame: &'a Frame` | built only in `resolve`/`resolve_owning`; its users call `resolve.frame()` (`told`, `release::emitted`, `statics`) |
| `v2/shape.rs` `Out` | `Out<'b>` | `of`, `value`, and helpers taking `&mut Out` (unchanged spelling) |
| `v2/shape.rs` `Data` | `Data<'f>` | `of`, `value` |
| `v2/side.rs` `Written*` | new, `'a`/`'f` | `encode`, `ranges`, `inlined_ranges`, `extend` |
| `blaze-codegen` `Shape(Vec<u8>)` | becomes `Shape(ContentId)`, `Copy` | `shape::of`, `driver.rs:777`, `driver/tests.rs:151` |
| `TemplateView<'a>`, `DataView<'a>`, `UnwindView<'a>` | new | `Template::view`, `blob::decode_view`, `stored_function` |

New or changed function parameters:

- `convert::function(frame, ..)`, `convert::value(frame, ..)`;
- `side::of(.., frame)`, `side::encode(.., frame)`;
- `shape::of(unit, function, frame)`;
- `named(.., frame)`, `scoped(.., frame)`, `colocate(.., frame)`;
- `emit_with(ctx, store, relocs, lines)`;
- `Lookup::get(key) -> Result<Option<Vec<u8>>, String>` becomes
  `Lookup::read(key) -> Result<Option<&[u8]>, String>`, borrowed from the store, with no callback
  and no new `dyn`;
- `Out::side(&mut Emitted)`, `Out::template_of(Emitted)`.

Every caller is listed with its change above.

> forwarded to codegen (`hir-ty-codegen`, `blaze-codegen`) (from requests, pass 17; told by tmux).

## 4. Boundary with `blaze-engine` (agreed with `mem-codegen-engine`)

`mem-codegen-engine` (window `mem-codegen-engine-next1`) proposed this boundary, and this
section takes it as given:
- The backend, `hir-ty-codegen`, opens the per-unit `frame::task(Tag::Codegen)` inside its
  worker job loops (`units.rs:365`, `mod.rs:6434`). `blaze-engine` opens no frame.
- No `Frame` crosses `SymbolResolver`.
- The seven methods in 4.2 change so that a warm unit's lookups allocate nothing.

Line numbers are from the tree of 2026-10-29, 20:45. `mod.rs` is being edited concurrently, so
find each site by the code quoted.

> forwarded to codegen (codegen-next1: `crates/hir-ty-codegen/`, `crates/blaze-engine/`) (requests, pass 18; told by tmux).

### 4.1 Where the frames open

**This request opens the per-unit frame in `hir-ty-codegen`'s worker job loops, not in
`blaze-engine`.** That matches what `mem-codegen-engine` assumes, and what its request for
`blaze-engine` leaves out.

Why the loops: each `ParallelWork` the engine's `in_parallel_beside` runs
(`blaze-engine/src/lib.rs:1428`) is a whole worker's job loop, not one unit. The engine can't see
where one unit ends, so `blaze-engine` opens no `frame::task`.

There are two loops, and each opens `blaze_mem::frame::task(blaze_mem::Tag::Codegen, ..)` once
per job, that is once per unit:

1. `units.rs` `Context::work` (547), which `ObjectCodegen::everywhere` (`units.rs:366`) runs on
   every core and on the calling thread. See 4.1.1.
2. `mod.rs` `compile_ahead`'s `run` loop, which its `work` closure (about 6434) runs. See 4.1.2.

**Nested frames.** The frames of sections 1.4, 1.6, 1.8 and 1.9 open inside a unit's frame as
the unit compiles each function. These are `build()`, `prepared`, `resolve`/`resolve_owning`
and `shape::value`. Each takes the next frame of the thread's stack and gives it back when its
function is done. So at most five are open at once (job, then `build` or `prepared`, then
`resolve`, then `shape::value`), within `blaze_mem::frame::DEPTH` (8). They stay as written, so each function
gives its frame back as soon as it is done, whichever path reaches it.

**What leaves a unit's frame:** nothing. Every value that leaves a job is built on the global
heap and outlives the frame:
- `built`'s `(Option<Result<Produced>>, Found)` and `compiled`'s
  `(Vec<(Kind, Outcome)>, Found)`, which go into the round's shared state (`state.built[at]`,
  `state.done`) for another thread to read;
- `run`'s `found` and `called` elements, which move into `state.queue` and `state.roots`.

Each such allocation keeps the note `// alloc: handed to the round, read by its owner after the
job`.

#### 4.1.1 `units.rs` `Context::work`

> applied by codegen-v1 (2026-10-29): as written (`units.rs` is v1's): both job loops in `Context::work` open a task, `built` takes the frame, and `calls_after_imports` is on it with `sort_copy_by_key_in`.

Before (553-579):

```rust
                drop(state);
                let built = self.built(db, symbols, &names, &mut homes, &kind);
                state = lock();
                ...
            if let Some(Queued { place, job, .. }) = state.jobs.pop() {
                drop(state);
                let (outcomes, found) = self.compiled(db, symbols, &names, &mut homes, job);
                state = lock();
```

After:

```rust
                drop(state);
                // The unit's frame: what building its IR needs and drops (`calls_after_imports`).
                let built = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                    self.built(db, symbols, &names, &mut homes, &kind, frame)
                });
                state = lock();
                ...
            if let Some(Queued { place, job, .. }) = state.jobs.pop() {
                drop(state);
                // The unit's frame: each member's `build()` nests its own in it.
                let (outcomes, found) = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |_| {
                    self.compiled(db, symbols, &names, &mut homes, job)
                });
                state = lock();
```

`built` gains `frame: &Frame` as its last parameter and passes it to `calls_after_imports`, at
its one call (631):

```rust
            Some(Ok(Produced::Pending(pending))) if shares(pending).is_ok() => {
                let calls = calls_after_imports(db, pending, frame);
                self.finder.find(db, symbols, names, homes, &calls, &mut found);
            }
```

`find` takes `impl IntoIterator<Item = &FuncRequest>`, and `&Vec<FuncRequest, &Frame>` is one.

`compiled`'s frame holds nothing of `compiled` itself. `unit(db, members)` and `find` build only
what leaves the job. What the unit's members take, they take in their own nested frames. This
frame is the boundary `mem-codegen-engine` relies on: when it closes, everything the unit took
from frames has gone back.

`calls_after_imports` (`units.rs:189`) moves onto the frame. Before:

```rust
fn calls_after_imports(db: &Db, pending: &Pending) -> Vec<FuncRequest> {
    let (Ok(isa), Ok(target)) = (super::target(), super::selected_target()) else {
        return pending.wants.requests.funcs.clone();
    };
    ...
    let imported: FxHashMap<u32, &imports::Import> = chosen.iter().map(|it| (it.id, it)).collect();
    ...
    let (mut next, mut addressed, mut data) = (Vec::new(), FxHashSet::default(), Vec::new());
    ...
    let (mut called, mut seen, mut reached) =
        (FxHashSet::default(), FxHashSet::default(), FxHashSet::default());
    ...
    let mut called: Vec<u32> = called.into_iter().collect();
    blaze_ids::radix::sort(&mut called);
    called.into_iter().map(|at| requests.funcs[at as usize].clone()).collect()
}
```

After:

```rust
fn calls_after_imports<'f>(db: &Db, pending: &Pending, frame: &'f Frame) -> Vec<FuncRequest, &'f Frame> {
    let (Ok(isa), Ok(target)) = (super::target(), super::selected_target()) else {
        let all = &pending.wants.requests.funcs;
        let mut calls = Vec::with_capacity_in(all.len(), frame);
        calls.extend(all.iter().cloned());
        return calls;
    };
    ...
    let mut imported = frame_map::hash_map_with_capacity(chosen.len(), frame);
    imported.extend(chosen.iter().map(|it| (it.id, it)));
    ...
    let (mut next, mut addressed, mut data) =
        (Vec::new_in(frame), frame_map::hash_set(frame), Vec::new_in(frame));
    ...
    let (mut called, mut seen, mut reached) =
        (frame_map::hash_set(frame), frame_map::hash_set(frame), frame_map::hash_set(frame));
    ...
    let mut sorted: Vec<u32, &Frame> = Vec::with_capacity_in(called.len(), frame);
    sorted.extend(called);
    blaze_mem::scratch::with_u32s(|scratch| {
        blaze_ids::radix::sort_copy_by_key_in(&mut sorted, |&it| it, scratch);
    });
    let mut calls = Vec::with_capacity_in(sorted.len(), frame);
    calls.extend(sorted.iter().map(|&at| requests.funcs[at as usize].clone()));
    calls
}
```

The lines marked `...` are unchanged. `imported` keeps its type,
`FrameHashMap<'f, u32, &imports::Import>`.

`units.rs` gains `use blaze_mem::Frame;` and `use super::frame_map;`; `frame_map` is the module
that 1.2 adds to `mod.rs`. If `FxHashSet` and `FxHashMap` have no other use left in `units.rs`
(`rg -n 'FxHash' units.rs`), their imports go.

The output is the same:
- `seen` and `reached` are only tested.
- `addressed` is iterated, but only into the worklist `next` and into `called`. What the loop
  reaches from a set of starting points doesn't depend on the order it takes them in.
- `called` becomes the sorted, deduplicated list of reached IDs, as before: an unstable radix
  sort of `u32`s by themselves is the same ascending list.

The radix sort changes because `radix::sort` allocates its scratch on every call.
`sort_copy_by_key_in` (`crates/blaze-radix/src/lib.rs:196`) uses the thread's `u32` buffer.

`Recorder::new`, `moved_requests`, `imports::chosen_for` and `recorder.wants.into_requests()`
still allocate on the global heap: they are the recorder's own types, which hold `Vec`s and
maps without an allocator parameter. They keep `// alloc: the import recorder's tables,
per unit`. The `FuncRequest` clones allocate what their `Instance`s own, as `clone` did before.

#### 4.1.2 `mod.rs` `compile_ahead`'s `run`

> applied by codegen-v1 (2026-10-29): as written, except that the task's closure takes the lock again and returns the guard (`state = frame::task(..)`): `state` is moved by `drop(state)` before the job, so a closure can't capture it to assign it.

This loop's frame holds the job's own temporaries, as below.

Before (`mod.rs`, in `run`'s `loop`, after `drop(state);`):

```rust
                let working = Working(&ahead, &waits);
                let (mut found, mut called) = (Vec::new(), Vec::new());
                let compiled = match &job {
                    ...
                };
                // The thread that puts the function in the object asks for the value of each
                // static it names and the place of each panic: it would find them one by one.
                let mut statics = Vec::new();
                ...
                let mut warmed = FxHashSet::default();
                while let Some(id) = statics.pop() {
                    ...
                }
                ...
                progress.notify_all();
            }
```

After:

```rust
                blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                    let working = Working(&ahead, &waits);
                    let (mut found, mut called) = (Vec::new_in(frame), Vec::new_in(frame));
                    let compiled = match &job {
                        ... // unchanged
                    };
                    // The thread that puts the function in the object asks for the value of
                    // each static it names and the place of each panic: it would find them one
                    // by one.
                    let mut statics = Vec::new_in(frame);
                    ... // unchanged
                    let mut warmed = frame_map::hash_set(frame);
                    while let Some(id) = statics.pop() {
                        ... // unchanged
                    }
                    ... // unchanged, through `state = ahead.lock()..` and the pushes
                    progress.notify_all();
                });
            }
```

The closure moves `job` in and borrows `state` mutably. It reassigns `state = ahead.lock()..`
as the body does today, and the loop reads `state` again at its top. `found`, `called` and
`statics` keep the order they are pushed in. `for it in found` and `for root in called` move each
element into `state.queue` and `state.roots`, which are global `VecDeque`s, so nothing in the
frame outlives the job. `warmed` is only inserted into, never iterated.

If the job panics, `Working` and `Leaving` drop as they do today, and the frame resets on
unwind.

### 4.2 `SymbolResolver` (`mod.rs:339`)

> applied by codegen-v1 (2026-10-29): the trait as written; the engine's and blaze-wasm's implementors are requested in `mem-codegen-engine.md` and 4.5.

`intern::Symbol` is `Copy` and four bytes, and `as_str()` gives a `&'static str`
(`crates/intern/src/symbol.rs:54`, 190). `hir-ty-codegen` already depends on `intern`
(`Cargo.toml:47`).

Before:

```rust
pub trait SymbolResolver {
    fn function(&self, func: FunctionId) -> Option<String>;
    fn static_(&self, id: StaticId) -> Option<String>;
    ...
    fn path(&self, path: &str) -> Option<String>;
    ...
    fn source_path(&self, file: base_db::FileId) -> Option<String>;
    ...
    fn closure(&self, _owner: FunctionId, _expr: hir_def::hir::ExprId) -> Option<String> {
        None
    }
    ...
    fn makers(&self, _function: &str) -> Vec<u64> {
        Vec::new()
    }
    ...
    fn copies(&self, _symbol: &str) -> Vec<(String, u64)> {
        Vec::new()
    }
```

After:

```rust
pub trait SymbolResolver {
    fn function(&self, func: FunctionId) -> Option<intern::Symbol>;
    fn static_(&self, id: StaticId) -> Option<intern::Symbol>;
    ...
    /// A non-generic function by path, e.g. `core::panicking::panic_bounds_check`.
    fn path(&self, path: &str) -> Option<&str>;
    ...
    /// The name rustc gives a source file in panic messages.
    fn source_path(&self, file: base_db::FileId) -> Option<intern::Symbol>;
    ...
    /// rustc's symbol of the closure at `expr` in the body of `owner`, which is not generic.
    fn closure(&self, _owner: FunctionId, _expr: hir_def::hir::ExprId) -> Option<intern::Symbol> {
        None
    }
    ...
    /// Adds to `out` the `StableCrateId` of each crate that compiled a copy of an instance of a
    /// function named `function`. The caller lends `out` empty, from a buffer it keeps.
    fn makers(&self, _function: &str, _out: &mut Vec<u64>) {}
    ...
    /// Adds to `out` the copies rustc made of the instance `symbol` is one of, each with the
    /// `StableCrateId` of the crate that compiled it, in the order the table has them. The
    /// caller lends `out` empty, from a buffer it keeps.
    fn copies(&self, _symbol: &str, _out: &mut Vec<(intern::Symbol, u64)>) {}
```

The other methods keep their signatures, including `item`, which returns `String` and is not on
a unit's warm path. `ParallelWork` (`mod.rs`, after the trait) is unchanged.

### 4.3 Its callers in `hir-ty-codegen`

> applied by codegen-v1 (2026-10-29): every listed site, plus the `root_named` caller in `found_callee` (`symbol.as_str()`).

`rg -n '\b(symbols|resolver)\s*\.\s*(function|static_|source_path|path|closure|makers|copies)\('`
lists them. `names.static_` at `mod.rs` 6385 and `later.rs` 559 is `StableNames`', not the
resolver's, and is unchanged.

No change: these only test `is_some()` or `is_none()`, or ignore the value:

- `mod.rs`: 1052, 6351 (`.and_then(|_| ..)`), 6411;
- `units.rs`: 156, 382, 601;
- `mod.rs` 5804: `unsupported!("`{symbol}`..")` formats it through `Symbol`'s `Display`
  (`symbol.rs:245`).

**`function`**

| site | before | after |
| --- | --- | --- |
| `mod.rs` 1218 | `cg.define_refused(&symbol, &e.to_string())?;` | `cg.define_refused(symbol.as_str(), &e.to_string())?;` |
| `mod.rs` 5816-5829 (`define_function`, stored) | `self.module.declare_function(&symbol, ..)` and `return Ok(symbol);` | `self.module.declare_function(symbol.as_str(), ..)` and `return Ok(symbol.as_str().to_owned()); // alloc: the caller keeps it, once per root` |
| `mod.rs` 5832 (`define_function`, compiled) | `let Some(symbol) = self.symbols.function(func) else {` | the same, followed by `let symbol = symbol.as_str().to_owned(); // alloc: kept by later::Of::Root and returned, once per root`; the rest is unchanged |
| `mod.rs` 6772 | `.is_some_and(\|it\| self.symbols.defines(&it))` | `.is_some_and(\|it\| self.symbols.defines(it.as_str()))` |
| `mod.rs` 6800 (`root_named`) | `-> Option<(FunctionId, String)>` | `-> Option<(FunctionId, intern::Symbol)>`; the body is unchanged |
| `mod.rs` 6864 | `self.import(&symbol, &func.sig)?` | `self.import(symbol.as_str(), &func.sig)?` |
| `later.rs` 391 (`root_named`) | `-> Option<(FunctionId, String)>` | `-> Option<(FunctionId, intern::Symbol)>`; its callers (465, 477, 499) only test `is_some`/`is_none` |
| `mod.rs` 7528-7542 (`callee`) | `self.rustc_copy(symbol, instance.checked)` and `self.import(&symbol, sig)` | `self.rustc_copy(symbol.as_str(), instance.checked)` and `self.import(symbol, sig)` |
| `patch.rs` 1600-1609 | `self.compiled.push((root, symbol));` | `self.compiled.push((root, symbol.as_str().to_owned())); // alloc: the patch's base keeps it` |

**`static_`**

| site | before | after |
| --- | --- | --- |
| `mod.rs` 7247 | `let mut symbol = self.symbols.static_(id);` | the same, now `Option<intern::Symbol>` |
| `mod.rs` 7254 | `if let Some(found) = symbol.as_deref()` | `if let Some(found) = symbol.map(\|it\| it.as_str())` |
| `mod.rs` 7278 | `symbol.as_ref().is_some_and(\|it\| self.symbols.linked(it))` | `symbol.is_some_and(\|it\| self.symbols.linked(it.as_str()))` |
| `mod.rs` 7301 | `rustcs.as_ref().is_some_and(\|it\| !self.symbols.linked(it))` | `rustcs.is_some_and(\|it\| !self.symbols.linked(it.as_str()))` |
| `mod.rs` 7302 | `let theirs = \|\| rustcs.filter(\|_\| !unlinked);` | `let theirs = \|\| rustcs.filter(\|_\| !unlinked).map(\|it\| it.as_str().to_owned()); // alloc: once per static linked to rustc's copy; static_link_name gives a String` |
| `mod.rs` 7407-7411 | `let own = if cfg!(feature = "wasm") { self.symbols.static_(id) } else { None };` and `Some(name) => name,` | the same `let own`, and `Some(name) => name.as_str().to_owned(), // alloc: wasm only, once per static` |
| `patch.rs` 1629 | `symbol: symbols.static_(id).unwrap_or_default(),` | `symbol: symbols.static_(id).map_or_else(String::new, \|it\| it.as_str().to_owned()), // alloc: the patch's base keeps it` |

**`source_path`**

| site | before | after |
| --- | --- | --- |
| `mod.rs` 7201, `later.rs` 163 | `symbols.source_path(file).map(\|it\| as_rustc_spells(&it))` | `symbols.source_path(file).map(\|it\| as_rustc_spells(it.as_str()))`; still once per file, kept in `source_paths` |
| `stable.rs` 244 | `self.stored(self.symbols.source_path(file)?)` | `self.stored(self.symbols.source_path(file)?.as_str().to_owned()) // alloc: stored gives an owned name` |
| `stable.rs` 883 | `named(symbols.source_path(file.file_id(db)))` | `named(symbols.source_path(file.file_id(db)).map(\|it\| it.as_str().to_owned()))` with the same `// alloc:` note |

At these three sites the count is the same as today, where the resolver allocated the `String`.
What still allocates is `StableNames::stored`/`named` giving owned names, which is
`StableNames`' and not the boundary's.

**`closure`**

| site | before | after |
| --- | --- | --- |
| `patch.rs` 1813-1818 | `Some(symbol) => { let known = BaseInstance { env: env.clone(), symbol, ..` | `Some(symbol) => { let known = BaseInstance { env: env.clone(), symbol: symbol.as_str().to_owned(), .. // alloc: the patch's base keeps it` |

**`path`**

The result is `&'a str`, borrowed from the resolver, not from the key. So a key built in a
scratch string can be dropped before the result is used. Where the caller then needs
`&mut self`, it copies the resolver out first: `let symbols = self.symbols;`.

| site | before | after |
| --- | --- | --- |
| `mod.rs` 6046 | `let Some(rustcs) = self.symbols.path(&format!("__rustc::{name}")) else {` | `let symbols = self.symbols;` before the loop, then `let Some(rustcs) = blaze_mem::scratch::with_string(\|key\| { _ = write!(key, "__rustc::{name}"); symbols.path(key) }) else {`. Later, `self.define_forwarding(&rustcs, ..)` becomes `self.define_forwarding(rustcs, ..)` |
| `mod.rs` 7106 | `self.import(&symbol, &request.sig)` | `self.import(symbol, &request.sig)`, with `let symbols = self.symbols;` and `symbols.path(path)` at 7106 |
| `mod.rs` 7598 | `if let Some(symbol) = self.symbols.path(&format!("<core::any::TypeId>::of::<{ty}>")) {` / `return self.import(&symbol, sig);` | `let symbols = self.symbols;` and `if let Some(symbol) = blaze_mem::scratch::with_string(\|key\| { _ = write!(key, "<core::any::TypeId>::of::<{ty}>"); symbols.path(key) }) {` / `return self.import(symbol, sig);` |
| `mod.rs` 7603-7611 | `self.symbols.path(&format!("<{ty} as core::any::Any>::type_id"))` and `self.import(&symbol, &method)?` | the same `with_string` form with `"<{ty} as core::any::Any>::type_id"`, and `self.import(symbol, &method)?` |
| `mod.rs` 7644 | `self.symbols.path(&format!("__rustc::{name}"))` and `self.import(&symbol, sig)` | the `with_string` form with `"__rustc::{name}"`, and `self.import(symbol, sig)` |
| `mod.rs` 7981 | `self.symbols.path(&format!("<{}>::{name}::<{ty}>", self.symbol_type(self_ty)?))` | `let of = self.symbol_type(self_ty)?;` then `blaze_mem::scratch::with_string(\|key\| { _ = write!(key, "<{of}>::{name}::<{ty}>"); self.symbols.path(key) })`. `any_method`'s return type goes from `Option<String>` to `Option<&'a str>` (`'a` is `ObjectCodegen`'s, the lifetime of `symbols: &'a dyn SymbolResolver`, 2236). Its one caller (7578) goes from `self.import(&symbol, sig)` to `self.import(symbol, sig)` |
| `mod.rs` 7808 (`stands_in`) | `.and_then(\|path\| self.symbols.path(&path))` | unchanged in form: `.and_then(\|path\| self.symbols.path(&path))` now gives `&str` to `rustc_copy` |
| `mod.rs` 7856-7859 (`forward_to_rustc`) | `let symbol = self.symbols.path(&..)?; let symbol = self.rustc_copy(symbol, ..)?; .. self.import(&symbol, &sig)` | `let symbols = self.symbols;` and `symbols.path(&..)?`, `self.rustc_copy(symbol, ..)?`, `self.import(symbol, &sig)` |

`write!` into a `String` needs `std::fmt::Write`. `mod.rs` imports it only inside one function
(2116), so each of these functions gains `use std::fmt::Write as _;`.

At 7808, 7856 and 6756 the key is `self.instance_path(..)`, which returns a `String` from
`Spelling::instance_path` (`mod.rs:1916`). That `String` stays: `// alloc: Spelling writes an
owned path`. Moving `Spelling` to write into a lent buffer changes `Spelling`'s callers
throughout, and is outside this boundary.

**`makers` (`mod.rs` 6713-6717, `settled`)**

Before:

```rust
            let makers = home::written_in(db, instance).map(|func| {
                let name = FunctionSignature::of(db, func).name.as_str();
                let makers = self.symbols.makers(name).into_iter();
                makers.filter_map(|id| mixed.holder(db, id)).collect::<Vec<_>>()
            });
```

After:

```rust
            let makers = home::written_in(db, instance).map(|func| {
                let name = FunctionSignature::of(db, func).name.as_str();
                blaze_mem::scratch::with_u64s(|makers| {
                    self.symbols.makers(name, makers);
                    // alloc: only a program whose crates check overflow differently gets here
                    makers.iter().filter_map(|&id| mixed.holder(db, id)).collect::<Vec<_>>()
                })
            });
```

**`copies`**

`settled_function`, `mod.rs` 6756-6759. Before:

```rust
            let copies = self.symbols.path(&path).map(|it| self.symbols.copies(&it));
            let holders: Vec<_> =
                copies.iter().flatten().filter_map(|(_, id)| mixed.holder(db, *id)).collect();
```

After:

```rust
            let holders: Vec<_> = spare::with_copies(|copies| {
                if let Some(it) = self.symbols.path(&path) {
                    self.symbols.copies(it, copies);
                }
                // alloc: only a program whose crates check overflow differently gets here
                copies.iter().filter_map(|&(_, id)| mixed.holder(db, id)).collect()
            });
```

`rustc_copy`, `mod.rs` 6783-6797. Before:

```rust
    fn rustc_copy(&self, symbol: String, checked: bool) -> Option<String> {
        let linked = |it: &str| self.symbols.linked(it);
        let Some(mixed) = &self.mixed else {
            if linked(&symbol) {
                return Some(symbol);
            }
            return self.symbols.copies(&symbol).into_iter().map(|it| it.0).find(|it| linked(it));
        };
        let copies = self.symbols.copies(&symbol);
        if copies.is_empty() {
            return Some(symbol).filter(|it| linked(it));
        }
        let of = |id: u64| mixed.holder(self.db, id).is_some_and(|it| it.1 == checked);
        copies.into_iter().find(|it| of(it.1) && linked(&it.0)).map(|it| it.0)
    }
```

After:

```rust
    fn rustc_copy<'s>(&self, symbol: &'s str, checked: bool) -> Option<&'s str> {
        let linked = |it: &str| self.symbols.linked(it);
        let Some(mixed) = &self.mixed else {
            if linked(symbol) {
                return Some(symbol);
            }
            return spare::with_copies(|copies| {
                self.symbols.copies(symbol, copies);
                copies.iter().map(|it| it.0.as_str()).find(|it| linked(it))
            });
        };
        spare::with_copies(|copies| {
            self.symbols.copies(symbol, copies);
            if copies.is_empty() {
                return Some(symbol).filter(|it| linked(it));
            }
            let of = |id: u64| mixed.holder(self.db, id).is_some_and(|it| it.1 == checked);
            copies.iter().find(|it| of(it.1) && linked(it.0.as_str())).map(|it| it.0.as_str())
        })
    }
```

The checks run in the same order, and `find` returns the same first match. On the unmixed path,
`copies` is still asked only when `symbol` itself is not linked. A copy's `&'static str`
coerces to `&'s str`.

Its three callers (7530, 7809, 7857) pass a `&str` and use the result as one, as in the tables
above.

### 4.4 `spare::with_copies` and its test

> applied by codegen-v1 (2026-10-29): `spare::with_copies` and `a_kept_list_of_copies_is_reused`, as written.

`copies` lends a `Vec<(intern::Symbol, u64)>`. `blaze_mem::scratch` has no shelf of that type,
and its `Shelf` is private. So the buffer gets a shelf in `spare.rs`, beside `RELOCS` and
`LINES` (1.5).

In `spare.rs`'s `thread_local!`:

```rust
    static COPIES: RefCell<Vec<Vec<(intern::Symbol, u64)>>> = const { RefCell::new(Vec::new()) };
```

After `keep_lines`:

```rust
/// Lends `f` an empty list for [`SymbolResolver::copies`](super::SymbolResolver::copies): one
/// the thread kept, or a new one, kept again after.
pub(super) fn with_copies<R>(f: impl FnOnce(&mut Vec<(intern::Symbol, u64)>) -> R) -> R {
    let mut copies = COPIES.with_borrow_mut(Vec::pop).unwrap_or_default();
    copies.clear();
    let result = f(&mut copies);
    if copies.capacity() <= LARGEST_ROWS {
        copies.clear();
        COPIES.with_borrow_mut(|kept| {
            if kept.len() < KEPT {
                kept.push(copies);
            }
        });
    }
    result
}
```

`pop` and `push` let a call inside `f` take another list. A panic in `f` drops the list, and the
next call makes a new one.

`spare.rs`'s tests gain:

```rust
    /// A list of copies comes back with its room, and filling it again to what it held takes
    /// nothing of the global allocator.
    #[test]
    fn a_kept_list_of_copies_is_reused() {
        COPIES.with_borrow_mut(Vec::clear);
        let symbol = intern::Symbol::intern("_RNvCs1234_5crate4copy");
        let fill = |copies: &mut Vec<(intern::Symbol, u64)>| {
            copies.extend((0..64).map(|at| (symbol, at)));
            copies.len()
        };
        assert_eq!(with_copies(fill), 64);
        let held = blaze_alloc_count::assert_at_most(0, "a warm list of copies", || {
            with_copies(|copies| {
                assert!(copies.is_empty(), "a lent list is empty");
                fill(copies)
            })
        });
        assert_eq!(held, 64);
    }
```

`Symbol::intern` runs before the measured closure, so the interning is not counted.
`(0..64).map(..)` is `TrustedLen`, so `extend` reserves once, and the kept list already has room
for 64.

### 4.5 The other implementors

Each implementor changes with the trait. The engine's `Resolver` (`blaze-engine/src/lib.rs:1428`)
is `mem-codegen-engine`'s to request.

`blaze-wasm` is in neither lane: it goes to the coordinator, and `mem-codegen-engine` may fold it
into its request. Its two implementors change as follows:

- **`blaze-wasm/src/lib.rs` `impl SymbolResolver for Names<'_>` (509).**
  - `function` and `static_`: `Some(intern::Symbol::intern(&format!(..)))` in place of
    `Some(format!(..))`.
  - `source_path`: `Some(intern::Symbol::intern(root.path_for_file(&file)?.as_str()))`, if
    `VfsPath` has `as_str` (read it before writing), else `&root.path_for_file(&file)?.to_string()`.
  - `path`: `self.library.get_key_value(path).map(|(key, _)| key.as_str())`, where `library` is
    `&FxHashMap<String, FunctionId>` (488-491).
- **`blaze-wasm/src/names.rs` `impl SymbolResolver for Resolver<'_>` (258).**
  - `function`, `static_` and `source_path` intern what they build, as above.
  - `path`: `self.table.get(path)`, which is already `Option<&str>`.

The wasm path stays on the global heap until `blaze-mem` runs on wasm32 (section 0), so interning
there is `// alloc: wasm, per name`.
