# Memory changes for `crates/hir-ty-mir` (from `mem-codegen-mir`)

For: lane `codegen` and its child `codegen-mir` (owners of the crate), and `speed-hir-store` /
`speed-hir-store-paths-consumers` (their edits touch `lower.rs`; nothing here conflicts with a
path-type change). From `mem-codegen-mir`, child of `mem-codegen` in lane `mem`. Nothing in the
crate was edited; this file is the whole change.

Line numbers are of the tree on 2026-10-29 (`blaze-v2`); each change also quotes the code it
replaces, so it can be found after other edits. Rules it follows: `docs/v2/plan/lanes/mem/db.md`
(one frame per query run, owned memoized results built once at exact size, temporaries in the
frame, scratch for per-call buffers, a budget test per change) and lane mem's ruling in
`v2/requests/mem.md` (memoized results stay on the global allocator).

## 0. The shape of it

- **Frames.** Every unit of per-body work runs in `blaze_mem::frame::task(Tag::Mir, |frame| ..)`:
  - lowering a body (`lower_to_mir_with_store`, `promoted_mir_body_query`,
    `lower_closure_body`);
  - each pass of `run_passes` and `optimize`, one task per pass, so the frame is reset between
    passes and the peak is the largest pass, not their sum;
  - the per-body `Ctx` in an outer task around the passes, since its `needs_drop` cache lives
    as long as the body's pipeline;
  - `monomorphized`;
  - `frame_storage_conflicts`.

  A task inside the query executors' own per-run frame (mem-db's contract) just nests. Tasks
  are never opened per statement or per call.
- **What is kept.** The memoized `MirBody` is the query's result and stays owned (global
  allocator). Its one copy is made where it is made today, and only there:
  - `run_passes` (`transform.rs:133`, `let mut body = built.clone();`);
  - `optimize` (`optimize.rs:94`, `let mut body = body.clone();`);
  - `monomorphized` (`monomorphization.rs:382`, `let mut body = body.clone();`);
  - lowering, which builds blocks, locals and the body's lists in the frame and copies each into
    the result once, at its final size (§5.1, `MirLowerCtx::finish`).

  The passes then change that copy in place and rebuild a container only when they removed
  something from it.
- **Exact budget technique.** `MirBody` holds no `Arc`. Its heap parts are `Arena`s, `Vec`s,
  `ArenaMap`s, an `FxHashMap`, `Box<[Operand]>`s and spilled `SmallVec`s, and every type or
  constant in it is an interned handle. So `measure(|| body.clone()).allocations` is the number
  of heap blocks the body owns. A step that builds a body at exact size allocates exactly that
  many, plus the per-body setup that the front end owns (the `InferCtxt`, the `Resolver`). The
  tests measure that setup on its own and add it.
- **Per-call scratch.** The interpreter's per-call paths, the type-name formatter that codegen
  calls per function, and the tracing detail of `mir_body_query` stop allocating per call (§8,
  §9).
- **What depends on other crates** is in §11. Two of those changes are in `hir-ty`, and the
  exact lowering budget waits for them.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

## 1. Manifest and crate root

`crates/hir-ty-mir/Cargo.toml`, under `[dependencies]` after `blaze-ids.workspace = true`
(local deps), and under `[dev-dependencies]`:

```toml
# [dependencies], local deps
blaze-ids.workspace = true
blaze-mem.workspace = true
# [dependencies], next to rustc-hash
hashbrown.workspace = true
# [dev-dependencies]
blaze-alloc-count.workspace = true
```

`hashbrown` is for the frame maps (§2). They need hashbrown's `nightly` feature, so that its
`A: Allocator` is `core::alloc::Allocator` and `&Frame` is one. mem-db already filed that root
change against the workspace `hashbrown` entry (`Cargo.toml:294`, today `default-features = false,
features = ["inline-more"]`). Until it lands, §2's `FrameMap`/`FrameSet` don't compile. Nothing
else here needs it: the `Vec<T, &Frame>` and `Bits` changes don't.

`crates/hir-ty-mir/src/lib.rs`, after line 10
(`#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]`):

```rust
#![feature(allocator_api)]
```

`crates/hir-ty-mir/src/mir.rs`, in the module list (lines 18-29):

```rust
// before
mod transform;
pub mod type_name;
// after
mod transform;
pub mod type_name;
#[doc(hidden)]
pub mod budget;
mod framed;
```

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

## 2. New module `src/mir/framed.rs`: bit sets and maps in the frame

`optimize/dse.rs` and `transform/move_paths.rs` each have a `Bits(Vec<u64>)`; both go, and every
`FxHashSet<BasicBlockId>`/`FxHashSet<LocalId>` that a pass builds and drops becomes a `Bits`. The
per-body maps become `FrameMap`s.

```rust
//! What the passes keep while they run, in the task's frame: sets of block or local or move
//! path indices as words, and hash maps. A copy of a set is a bump of the frame, and all of
//! it goes when the task ends.

use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

/// A set of indices below the `len` it was made for. Bits from `len` on are always clear, so
/// two sets of one `len` are equal when their members are.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Bits<'f>(Vec<u64, &'f Frame>);

impl<'f> Bits<'f> {
    pub(crate) fn new_in(len: usize, frame: &'f Frame) -> Self {
        let words = len.div_ceil(64);
        let mut bits = Vec::with_capacity_in(words, frame);
        bits.resize(words, 0);
        Bits(bits)
    }

    pub(crate) fn contains(&self, at: usize) -> bool {
        self.0[at / 64] & (1 << (at % 64)) != 0
    }

    pub(crate) fn insert(&mut self, at: usize) {
        self.0[at / 64] |= 1 << (at % 64);
    }

    pub(crate) fn remove(&mut self, at: usize) {
        self.0[at / 64] &= !(1 << (at % 64));
    }

    /// Every index below `len`, the `len` the set was made for.
    pub(crate) fn insert_all(&mut self, len: usize) {
        self.0.fill(u64::MAX);
        if len % 64 != 0
            && let Some(last) = self.0.last_mut()
        {
            *last = (1 << (len % 64)) - 1;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.0.fill(0);
    }

    /// Adds the members of `other`, and says whether that changed anything.
    pub(crate) fn union(&mut self, other: &Bits<'_>) -> bool {
        let mut changed = false;
        for (word, &more) in self.0.iter_mut().zip(&other.0) {
            changed |= *word | more != *word;
            *word |= more;
        }
        changed
    }

    pub(crate) fn subtract(&mut self, other: &Bits<'_>) {
        for (word, &less) in self.0.iter_mut().zip(&other.0) {
            *word &= !less;
        }
    }

    pub(crate) fn intersect(&mut self, other: &Bits<'_>) {
        for (word, &with) in self.0.iter_mut().zip(&other.0) {
            *word &= with;
        }
    }

    /// `other`'s members in place of these, without allocating: both are of one `len`.
    pub(crate) fn copy_from(&mut self, other: &Bits<'_>) {
        self.0.copy_from_slice(&other.0);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.iter().all(|&it| it == 0)
    }

    pub(crate) fn words(&self) -> &[u64] {
        &self.0
    }

    /// The members, in increasing order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(at, &word)| {
            let mut rest = word;
            std::iter::from_fn(move || {
                (rest != 0).then(|| {
                    let bit = rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    at * 64 + bit
                })
            })
        })
    }
}

pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
pub(crate) type FrameSet<'f, K> = hashbrown::HashSet<K, FxBuildHasher, &'f Frame>;

pub(crate) fn map_in<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

pub(crate) fn set_in<'f, K>(frame: &'f Frame) -> FrameSet<'f, K> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}
```

Every set and map this replaces is only probed or iterated in an order that doesn't reach the
body (each such case is checked in its section), so the output is the same bytes. `FrameMap`
uses the same `FxBuildHasher` as the `FxHashMap` it replaces, so iteration order is also
unchanged where a map is iterated.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> codegen-mir (2026-10-29), on mem-codegen-mir's step-2 review: `framed.rs` follows the
> one-bit-set-family agreement with codegen-mir-store (its requests file): `Bits<'f, I>` is
> `blaze_ids::bits::BitSliceMut` over words from `bits_in(len, frame)`.
> - `is_empty` is `blaze_ids`' (`bits.rs:313`).
> - `Clone` can't be had on a `&mut [u64]` view. A copy is `let mut copy = bits_in(len, frame);
>   copy.copy_from(&set);`: one bump of the frame, as before.
> - `FrameMap`/`map_in` and `FrameSet`/`set_in` are in. codegen-mir-store added the set back
>   for `lower.rs`.
> - hashbrown's `nightly` is on in the root `Cargo.toml` (`features = ["inline-more",
>   "nightly"]`).

## 3. The runtime pipeline: `src/mir/transform.rs`

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 3.1 `Ctx` gains the frame (lines 47-129)

```rust
// before
pub(super) struct Ctx<'db> {
    pub(super) db: &'db Db,
    pub(super) infcx: InferCtxt<'db>,
    pub(super) env: ParamEnv<'db>,
    krate: Crate,
    needs_drop: RefCell<FxHashMap<Ty<'db>, bool>>,
}

impl<'db> Ctx<'db> {
    pub(super) fn new(db: &'db Db, owner: InferBodyId<'db>) -> Self {
        ..
        Ctx { db, infcx, env, krate, needs_drop: RefCell::default() }
    }
// after
pub(super) struct Ctx<'f, 'db> {
    pub(super) db: &'db Db,
    pub(super) infcx: InferCtxt<'db>,
    pub(super) env: ParamEnv<'db>,
    krate: Crate,
    /// The frame of the body's pipeline, which outlives each pass's own.
    pub(super) frame: &'f Frame,
    needs_drop: RefCell<FrameMap<'f, Ty<'db>, bool>>,
}

impl<'f, 'db> Ctx<'f, 'db> {
    pub(super) fn new(db: &'db Db, owner: InferBodyId<'db>, frame: &'f Frame) -> Self {
        ..
        Ctx { db, infcx, env, krate, frame, needs_drop: RefCell::new(framed::map_in(frame)) }
    }
```

The rest of the impl is unchanged; `impl<'db> Ctx<'db>` becomes `impl<'f, 'db> Ctx<'f, 'db>`.
Callers: every `ctx: &Ctx<'db>` in the crate becomes `ctx: &Ctx<'_, 'db>`. That is 66 sites:

| File | Sites |
|---|---|
| `transform/move_paths.rs` | 15 |
| `transform/asserts.rs` | 8 |
| `transform/packed_drops.rs` | 2 |
| `transform/elaborate_drops.rs` | 2 |
| `transform.rs` | 2 |
| `optimize/check_pointers.rs` | 7 |
| `optimize/check_enums.rs` | 8 |
| `optimize/inst_simplify.rs` | 7 |
| `optimize/copy_prop.rs` | 2 |
| `optimize/cfg.rs` | 1 |
| `optimize/inline.rs` (not compiled yet, §7) | 9 |
| `optimize/inline/integrate.rs` (not compiled yet, §7) | 2 |

`Elaborator<'a, 'db>`'s `ctx: &'a Ctx<'db>` becomes `&'a Ctx<'f, 'db>` (§4.7). `pretty.rs`'s
`Ctx<'db>` is another type and is untouched.

Imports: `use rustc_hash::FxHashMap;` (line 28) goes;
`use blaze_mem::{Frame, Tag, frame};` and `use super::framed;` come in.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 3.2 `run_passes` (lines 131-159): one task for the body, one per pass

```rust
// before
fn run_passes<'db>(db: &'db Db, built: &MirBody<'db>) -> Result<'db, MirBody<'db>> {
    let mut body = built.clone();
    let ctx = Ctx::new(db, body.owner);
    for (_, block) in body.basic_blocks.iter_mut() { .. FalseEdge .. }
    asserts::add_asserts(&ctx, &mut body);
    upvars::upvars_in_place(&mut body);
    body.promoted_constants = body
        .promoted_locals
        .iter()
        .map(|&(local, _)| local)
        .filter(|&local| ctx.is_plain(body.locals[local].ty.as_ref()))
        .collect();
    cleanup::remove_noop_landing_pads(&mut body);
    elaborate_drops::elaborate_drops(&ctx, &mut body)?;
    packed_drops::add_moves_for_packed_drops(&ctx, &mut body);
    body.phase = MirPhase::Runtime;
    body.shrink_to_fit();
    Ok(body)
}
// after
fn run_passes<'db>(db: &'db Db, built: &MirBody<'db>) -> Result<'db, MirBody<'db>> {
    // The copy the query keeps: the passes change it in place.
    let mut body = built.clone();
    frame::task(Tag::Mir, |outer| -> Result<'db, ()> {
        let ctx = Ctx::new(db, body.owner, outer);
        for (_, block) in body.basic_blocks.iter_mut() { .. FalseEdge, unchanged .. }
        frame::task(Tag::Mir, |frame| asserts::add_asserts(&ctx, &mut body, frame));
        frame::task(Tag::Mir, |frame| upvars::upvars_in_place(&mut body, frame));
        let plain = body
            .promoted_locals
            .iter()
            .filter(|&&(local, _)| ctx.is_plain(body.locals[local].ty.as_ref()))
            .count();
        let mut promoted_constants = Vec::with_capacity(plain);
        promoted_constants.extend(
            body.promoted_locals
                .iter()
                .map(|&(local, _)| local)
                .filter(|&local| ctx.is_plain(body.locals[local].ty.as_ref())),
        );
        body.promoted_constants = promoted_constants;
        frame::task(Tag::Mir, |frame| cleanup::remove_noop_landing_pads(&mut body, frame));
        frame::task(Tag::Mir, |frame| elaborate_drops::elaborate_drops(&ctx, &mut body, frame))?;
        frame::task(Tag::Mir, |frame| {
            packed_drops::add_moves_for_packed_drops(&ctx, &mut body, frame)
        });
        Ok(())
    })?;
    body.phase = MirPhase::Runtime;
    body.shrink_to_fit();
    Ok(body)
}
```

Why:
- `promoted_constants`: `collect()` from a `filter` starts small and grows, so the kept `Vec`
  had spare capacity that `shrink_to_fit` then reallocated. Counting first makes it one
  allocation at its final size. `is_plain` is a pure function, and its second call hits the
  `needs_drop` cache, so the list and its order are unchanged.
- `shrink_to_fit` stays: the passes that add blocks grow the kept body in place (§4), and
  shrinking is where those amortized `Vec`s get their exact size.

Add the hook the budget tests call, which runs one pass alone:

```rust
/// One of these passes alone, or drop elaboration's analyses without its rewrite, in `frame`.
pub(super) fn run_one<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    pass: super::budget::Pass,
    frame: &Frame,
) -> Result<'db, ()> {
    use super::budget::Pass;
    match pass {
        Pass::AddAsserts => asserts::add_asserts(ctx, body, frame),
        Pass::RemoveNoopLandingPads => cleanup::remove_noop_landing_pads(body, frame),
        Pass::ElaborateDrops => elaborate_drops::elaborate_drops(ctx, body, frame)?,
        Pass::DropAnalysis => elaborate_drops::analyse(ctx, body, frame),
        other => unreachable!("{other:?} is an optimization: `optimize::run_one` runs it"),
    }
    Ok(())
}
```

`elaborate_drops::analyse` is §4.7's first block (live blocks, move paths, both analyses, dead
unwinds), with its result dropped. It is the only new function, and it is what
`elaborate_drops` calls first, so the test measures the code that runs.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: §3.1 and §3.2 as written (`db: &'db Db`): `Ctx<'f, 'db>` with `frame` and the `needs_drop` `FrameMap`, `run_passes` in an outer task with one task per pass and `promoted_constants` counted first, and `transform::run_one` (it returns `NotSupported` for an optimization instead of `unreachable!`; `optimize::run_one` sends only the runtime passes to it).

## 4. The runtime passes: `src/mir/transform/*`

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 4.1 `asserts.rs`

`add_asserts(ctx, body)` becomes `add_asserts(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>,
frame: &Frame)`.
- Lines 49-50, the `unwinds` map collected from `body.panic_unwinds`: an `FxHashMap` collect
  becomes a `FrameMap` built with `framed::map_in(frame)` and then
  `reserve(body.panic_unwinds.len())` and `extend`. It is only probed.
- Line 52, `let ids: Vec<_> = body.basic_blocks.iter().map(|(id, _)| id).collect();` becomes
  `for index in 0..body.basic_blocks.len() { let id = block_id(index); .. }`. Blocks that
  `assert()` pushes are appended after the original ones. `ids` held only those, and so does the
  index range taken before the loop: `let blocks = body.basic_blocks.len();`, then
  `for index in 0..blocks`.
- Lines 54-66, where each block's statements are taken and rebuilt:
  - **Before:** `let statements = std::mem::take(&mut block.statements);`, then a push of each
    rewritten statement into the block's empty `Vec`, which regrows from zero.
  - **After:** the old statements move into a frame `Vec` reused across blocks
    (`pending.clear(); pending.extend(block.statements.drain(..));`, with
    `let mut pending: Vec<Statement, &Frame> = Vec::new_in(frame);` before the loop).
  - The block keeps its own `Vec` and capacity. Each rewritten statement is pushed back
    into it, and the `Vec` grows only past its old length, by the checks' setup statements.
  - A block that `assert()` splits off for the statements after a check gets
    `Vec::with_capacity(pending.len() - consumed)`, which is exact: every statement after the
    check goes into it.
- Lines 60 and 70, the per-statement `checks` `Vec`: one frame `Vec` made before the loop and
  `drain(..)`ed each statement.
- `Check.setup: Vec<Statement>` holds at most 3 statements (the length, the comparison, the
  bound). It becomes `ArrayVec<Statement, 3>` (`arrayvec` is already a dependency).
  `push` sites unchanged; the consumer `block.statements.extend(check.setup)` is unchanged.
- Lines 171, 176 and 178: `rewrite` returns `vec![statement]` per statement. It becomes
  `-> Statement`, and the caller pushes it. Every return is one statement (the three sites are
  `vec![one]`).
- Lines 365-373, the `index_spans` collect: a frame `Vec` (`Vec::with_capacity_in(n, frame)`).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: as written; `index_spans` is a frame `Vec<Option<MirSpan>>` by local index (last marker wins, as the map's collect had it), and a block `assert` splits off gets room for the statements after the check.

### 4.2 `cleanup.rs`

`remove_noop_landing_pads(body)` becomes `remove_noop_landing_pads(body, frame: &Frame)`.
- Line 11: `let mut noop: FxHashSet<BasicBlockId> = FxHashSet::default();` becomes
  `let mut noop = Bits::new_in(body.basic_blocks.len(), frame);`.
- `noop.contains(&id)` becomes `noop.contains(block_index(id))`, and `noop.insert(id)`
  becomes `noop.insert(block_index(id))`.
- It is only probed, so the output is unchanged.
- `optimize.rs:149` calls this, from the optimize pipeline, with the pass's frame (§6.1).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: the fixpoint is `noop_in(body, frame) -> Bits` over block indices; `remove_noop_landing_pads(body, frame)` and codegen's public `noop_landing_pads(body)` (one task, the kept `FxHashSet` at its size) both use it.

### 4.3 `upvars.rs`

`upvars_in_place(body)` becomes `upvars_in_place(body, frame: &Frame)`.
- Line 65, `let upvars: Vec<_> = ...collect();` becomes a frame `Vec`:
  `let mut upvars = Vec::with_capacity_in(n, frame); upvars.extend(..);`.
- `body.upvars_in_place`, the kept list, is built once with `Vec::with_capacity(count)` and
  then filled.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: as written.

### 4.4 `packed_drops.rs`

`add_moves_for_packed_drops(ctx, body)` becomes `(ctx, body, frame: &Frame)`. The `frame`
parameter is unused today and is kept for symmetry with the other passes.
- Line 18: `let ids: Vec<_> = body.basic_blocks.iter().map(|(id, _)| id).collect();` becomes
  `let blocks = body.basic_blocks.len(); for index in 0..blocks { let id = block_id(index); .. }`.
- The moved-to blocks it allocates are kept body.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: as written.

### 4.5 `move_paths.rs`: the dataflow

The hottest pass. It runs on every body with something to drop. Today it allocates:
- per statement per visit: `effects_at`'s `Vec`;
- per effect: `subtree()`'s `Vec`;
- per block visit and per edge: `InitState` clones;
- per `SwitchInt` visit: `variants` and `children`.

All of it moves into the frame and is reused.

**Types**
- `MovePath<'db>.children: Vec<(Step, PathId)>` becomes `Vec<(Step, PathId), &'f Frame>`, so
  `MovePath<'f, 'db>`.
- `MoveData<'db> { paths: Vec<MovePath<'db>>, locals: ArenaMap<LocalId, PathId> }` becomes:

  ```rust
  pub(super) struct MoveData<'f, 'db> {
      pub(super) paths: Vec<MovePath<'f, 'db>, &'f Frame>,
      /// By local index: its path.
      locals: &'f mut [Option<PathId>],
      frame: &'f Frame,
      /// Filled once the paths are all made, by `finish`: each path's subtree, as
      /// `subtree_all[subtree_start[p]..subtree_start[p + 1]]`.
      subtree_start: Vec<u32, &'f Frame>,
      subtree_all: Vec<PathId, &'f Frame>,
  }
  ```
- `pub(super) struct Bits(Vec<u64>)` (lines 390-420) goes. Its uses become `framed::Bits<'f>`;
  `Bits::new(len)` becomes `Bits::new_in(len, frame)`, and `union` keeps its `-> bool`.
- `InitState` becomes `InitState<'f> { maybe_init: Bits<'f>, maybe_uninit: Bits<'f> }`, and
  `bottom(len)` becomes `bottom(len, frame)`. Add:

  ```rust
  pub(super) fn copy_from(&mut self, other: &InitState<'_>) {
      self.maybe_init.copy_from(&other.maybe_init);
      self.maybe_uninit.copy_from(&other.maybe_uninit);
  }
  ```

**`gather(ctx, body, live)`** (line 72) becomes `gather(ctx, body, live: &Bits<'f>, frame: &'f Frame)`.
- `data.locals.insert(local, id)` becomes `data.locals[local_index(local)] = Some(id)`, with
  `locals: frame.alloc_slice_fill_with(body.locals.len(), |_| None)`.
- `children: Vec::new()` becomes `Vec::new_in(frame)`, at lines 81 and 227.
- At the end, `data.finish()` fills the subtree table: for each path in order, a BFS over
  `children`, written straight into `subtree_all`.

  ```rust
  fn finish(&mut self) {
      self.subtree_start = Vec::with_capacity_in(self.paths.len() + 1, self.frame);
      self.subtree_all = Vec::new_in(self.frame);
      for path in 0..self.paths.len() {
          let start = self.subtree_all.len();
          self.subtree_start.push(start as u32);
          self.subtree_all.push(path);
          let mut at = start;
          while at < self.subtree_all.len() {
              let parent = self.subtree_all[at];
              self.subtree_all.extend(self.paths[parent].children.iter().map(|&(_, id)| id));
              at += 1;
          }
      }
      self.subtree_start.push(self.subtree_all.len() as u32);
  }
  ```

**`subtree(&self, path) -> Vec<PathId>`** (lines 280-288) becomes

```rust
pub(super) fn subtree(&self, path: PathId) -> &[PathId] {
    let start = self.subtree_start[path] as usize;
    let end = self.subtree_start[path + 1] as usize;
    &self.subtree_all[start..end]
}
```

- The order is the same BFS order. `child()` is only called from `create()` during `gather`;
  `elaborate_drops` calls only `find_child`, so the table can't go stale.
- `debug_assert!(!self.subtree_start.is_empty())` goes in `subtree`.
- Callers that did `&data.subtree(p)` now pass `data.subtree(p)`.
- `holds_nothing` (line 299) becomes `self.subtree(path).iter().all(..)`.

**`subslice(..) -> Vec<u64>`** (lines 143-149) becomes `-> std::ops::Range<u64>`: `from..len - to`
when the array length is known and `from + to <= len`, `0..0` otherwise. `create` iterates it.

**`paths_of(&self, ctx, place) -> SmallVec<[PathId; 1]>`** (lines 240-261) becomes
`paths_of(&self, ctx, place, mut f: impl FnMut(PathId))`, calling `f` with each path in the
same order:
- `SmallVec::from_buf([current])` becomes `f(current)`;
- `return SmallVec::new()` becomes `return`;
- the subslice arm becomes
  `for index in self.subslice(..) { if let Some(it) = self.find_child(current, Step::Element(index)) { f(it) } }`.

Callers:
- `effects_at`: `for path in data.paths_of(ctx, place) { effects.push(..) }` becomes
  `data.paths_of(ctx, place, |path| effects.push(..))`.
- `edge_effects` (lines 570-586) becomes
  `edge_effects(ctx, data, body, from, to, f: impl FnMut(PathId))`, forwarding `f`.

**`effects_at(..) -> Vec<Effect>`** (lines 490-550) becomes
`effects_at(ctx, data, body, block, at, effects: &mut Vec<Effect, &Frame>)`. It pushes onto
`effects`, which the caller has cleared; the body is unchanged apart from `return effects;`
becoming `return;`.

`init_target` (lines 553-566) returns a `StoredPlace` clone for the common case. That is a
refcount bump, not an allocation (an interned handle), so it stays.

**A per-body effects table** replaces both the `FxHashMap<(BasicBlockId, usize), Vec<Effect>>`
in `InitAnalysis::run` and `dead_unwinds`' recomputation:

```rust
/// The effects of each statement and terminator of the blocks asked for, made the first time
/// a block is asked for: what `effects_at` says, kept for the analyses' later visits.
pub(super) struct Effects<'f> {
    /// By block index: where the block's locations start in `starts`, `u32::MAX` until asked.
    block_start: &'f mut [u32],
    /// By location: where its effects start in `all`; each block's run ends with one more.
    starts: Vec<u32, &'f Frame>,
    all: Vec<Effect, &'f Frame>,
}

impl<'f> Effects<'f> {
    pub(super) fn new_in(blocks: usize, frame: &'f Frame) -> Self {
        Effects {
            block_start: frame.alloc_slice_fill_with(blocks, |_| u32::MAX),
            starts: Vec::new_in(frame),
            all: Vec::new_in(frame),
        }
    }

    /// The effects at statement `at` of `block`, its terminator when `at` is the number of
    /// statements.
    pub(super) fn at<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        data: &MoveData<'_, 'db>,
        body: &MirBody<'db>,
        block: BasicBlockId,
        at: usize,
    ) -> &[Effect] {
        let index = block.into_raw().into_u32() as usize;
        if self.block_start[index] == u32::MAX {
            self.block_start[index] = self.starts.len() as u32;
            for at in 0..=body.basic_blocks[block].statements.len() {
                self.starts.push(self.all.len() as u32);
                let mut more = Vec::new_in(*self.all.allocator());
                effects_at(ctx, data, body, block, at, &mut more);
                self.all.extend(more);
            }
            self.starts.push(self.all.len() as u32);
        }
        let first = self.block_start[index] as usize + at;
        &self.all[self.starts[first] as usize..self.starts[first + 1] as usize]
    }
}
```

- Better still, `effects_at` pushes straight into `self.all`; the temporary `more` is only
  there because `effects_at` borrows `data` while `all` is borrowed mutably. If the owner
  prefers, give `effects_at` the `&mut Vec<Effect, &Frame>` that is `self.all` itself: it only
  pushes, so the order is the same.
- The original computes a block's effects the first time the block is visited, all of its
  locations in one go (the loop over `at` at line 668 and the terminator at 685). This computes
  the same values for the same blocks, so nothing is computed that wasn't. `elaborate_drops`'
  own per-site loop (`elaborate_drops.rs:129-157`) uses the same table, so each location's
  effects are computed once for the whole pass instead of up to four times (two analysis runs,
  `dead_unwinds`, the site loop).

**`live_blocks(body) -> Vec<bool>`** (lines 590-610) becomes `live_blocks(body, unreached: &Bits<'f>,
frame: &'f Frame) -> Bits<'f>`.
- `live` is a `Bits`, and `stack` is a frame `Vec`.
- The `unreached` `FxHashSet` (line 593) and its twin in `run` (line 630) become one `Bits` over
  block indices. `elaborate_drops` builds it once from `body.unreached` and passes it to both.

**`InitAnalysis`** becomes `InitAnalysis<'f> { entry: Vec<Option<InitState<'f>>, &'f Frame> }`.
`run` takes `effects: &mut Effects<'f>`, `unreached: &Bits<'f>`, `dead_unwinds: &Bits<'f>` and
`frame: &'f Frame`. The changes in its body:
- Line 631: `vec![None; n]` becomes `Vec::with_capacity_in(n, frame)` plus `resize_with(n, || None)`.
- Line 633: `start.set(&(0..len).collect::<Vec<_>>(), false);` becomes
  `start.maybe_uninit.insert_all(len);`. `maybe_init` is already empty in `bottom`, and
  `set(.., false)` removes from `maybe_init` and inserts into `maybe_uninit` for every path, so
  the result is the same.
- Lines 636 and 640: `start.set(&data.subtree(path), true)` becomes
  `start.set(data.subtree(path), true)`. The `subtrees: Vec<Vec<PathId>>` (line 640) goes;
  `&subtrees[p]` becomes `data.subtree(p)`.
- Lines 643-645: `pending` and `queued` become a frame `Vec` and a `Bits`.
- Line 649: `let mut state = entry[..].clone().expect(..)` becomes one
  `let mut state = InitState::bottom(len, frame);` before the loop, then
  `state.copy_from(entry[index(id)].as_ref().expect("a block is queued with a state"));`.
- `flow`'s `slot @ None => *slot = Some(state.clone())` stays. That is one frame bump per block,
  the first time it is reached.
- Line 692: `let mut unwound = state.clone();` becomes a second reused state, `unwound`, made
  before the loop: `unwound.copy_from(&state)`. Then `skip_init` gives
  `unwound.maybe_init.clear()` (was `= Bits::new(len)`), and `skip_uninit` gives
  `unwound.maybe_uninit.clear()`.
- Lines 702-714, the variant switch: `switch_on_variant` fills a `VariantSwitch<'f>` reused
  across visits, and `edges` fills a reused `Edges<'f>` (below). For each edge, `edge.copy_from(&state)`,
  then `edge.set(data.subtree(path), false)` for its inactive paths, then `flow`.
- Lines 716-730, the other edges: `let initialized = edge_effects(..)` and `state.clone()`
  become `edge.copy_from(&state)` followed by
  `edge_effects(ctx, data, body, id, to, |path| { any = true; edge.set(data.subtree(path), true) })`,
  then `flow(to, if any { &edge } else { &state }, ..)`.
  - `edge` is a third reused state.
  - `edge_effects`' paths are disjoint from what `set` changes, so applying them as they come
    is the same as collecting them first.

**`dead_unwinds`** (lines 738-764) returns `Bits<'f>` over block indices.
- `entry.clone()` becomes `state.copy_from(entry)` into one reused state.
- `effects_at` and `subtree` per effect become `effects.at(..)` and `data.subtree(..)`.

**`switch_on_variant`** (lines 307-355) becomes
`switch_on_variant(&self, ctx, body, block, out: &mut VariantSwitch<'_>) -> bool`. It clears and
fills `out.variants` and `out.children`, both `Vec<_, &'f Frame>`, and returns `false` where it
returned `None`. The `const_eval_discriminant` calls are salsa hits after the first visit.
**`VariantSwitch::edges`** (lines 368-387) becomes `edges(&self, targets, out: &mut Edges<'_>)`:

```rust
/// The edges of a switch, each with the paths that hold nothing along it.
pub(super) struct Edges<'f> {
    /// (target, end of its paths in `paths`)
    to: Vec<(BasicBlockId, u32), &'f Frame>,
    paths: Vec<PathId, &'f Frame>,
}

impl Edges<'_> {
    pub(super) fn iter(&self) -> impl Iterator<Item = (BasicBlockId, &[PathId])> {
        let mut start = 0;
        self.to.iter().map(move |&(to, end)| {
            let paths = &self.paths[start..end as usize];
            start = end as usize;
            (to, paths)
        })
    }

    fn push(&mut self, to: BasicBlockId, paths: impl Iterator<Item = PathId>) {
        self.paths.extend(paths);
        self.to.push((to, self.paths.len() as u32));
    }

    fn clear(&mut self) {
        self.to.clear();
        self.paths.clear();
    }
}

// in VariantSwitch
pub(super) fn edges(&self, targets: &SwitchTargets, out: &mut Edges<'_>) {
    let variant_of =
        |value: u128| self.variants.iter().find(|&&(_, it)| it == value).map(|&(it, _)| it);
    out.clear();
    for (value, to) in targets.iter() {
        let Some(active) = variant_of(value) else {
            out.clear();
            for &to in targets.all_targets() {
                out.push(to, std::iter::empty());
            }
            return;
        };
        out.push(to, self.children.iter().filter(|&&(it, _)| it != active).map(|&(_, p)| p));
    }
    let named = |variant| targets.iter().any(|(value, _)| variant_of(value) == Some(variant));
    out.push(
        targets.otherwise(),
        self.children.iter().filter(|&&(it, _)| named(it)).map(|&(_, p)| p),
    );
}
```

`named` recomputes the original `named` `Vec`'s membership: by then every value had a variant,
or the function returned early. The edges, their order and their paths are the same, and so is
the cost: `named.contains` was already linear.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-drops (2026-10-29), uncompiled: all of §4.5 is in
> `move_paths.rs` on `framed::Bits` (no `Bits::clone`: `InitState::copy_in(frame)` makes the
> one copy per block), `effects_at` pushes straight into `Effects::all`, and a local
> `successors()` iterator replaces `TerminatorKind::successors()`, whose `SmallVec` spills on a
> switch of more than two targets.

### 4.6 What the tests hold `move_paths` to

The `Pass::DropAnalysis` warm test (§10, `warm_runtime_analyses_allocate_nothing`): live blocks,
`gather`, both `InitAnalysis::run`s and `dead_unwinds` allocate 0 on the global allocator, on a
body with moves, drops, a conditional move and a `match` on an enum.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-drops (2026-10-29), uncompiled: `elaborate_drops::analyse` is
> what `Pass::DropAnalysis` runs (returning `()`, so `run_one`'s arms agree); the warm test
> itself is §10.3's text for `tests/mir/alloc_budget.rs` (codegen-mir-store's file), and
> `move_paths.rs` adds database-free unit tests (subtree table against the old walk, `Edges`,
> `InitState`).

### 4.7 `elaborate_drops.rs`

`elaborate_drops(ctx, body)` becomes `elaborate_drops(ctx, body, frame: &Frame)`, with
`Elaborator<'a, 'f, 'db>`. Its first lines are split out as `analyse`, which `elaborate_drops`
calls:

```rust
pub(super) struct Analysed<'f, 'db> {
    data: MoveData<'f, 'db>,
    analysis: InitAnalysis<'f>,
    effects: Effects<'f>,
}

pub(super) fn analyse<'f, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    frame: &'f Frame,
) -> Analysed<'f, 'db> {
    let blocks = body.basic_blocks.len();
    let mut unreached = Bits::new_in(blocks, frame);
    for &it in &body.unreached {
        unreached.insert(block_index(it));
    }
    let live = move_paths::live_blocks(body, &unreached, frame);
    let data = MoveData::gather(ctx, body, &live, frame);
    let mut effects = Effects::new_in(blocks, frame);
    let none = Bits::new_in(blocks, frame);
    let analysis = InitAnalysis::run(ctx, &data, body, &live, &unreached, &none, &mut effects, frame);
    let dead_unwinds = analysis.dead_unwinds(ctx, &data, body, &mut effects, frame);
    let analysis = match dead_unwinds.is_empty() {
        true => analysis,
        false => InitAnalysis::run(
            ctx, &data, body, &live, &unreached, &dead_unwinds, &mut effects, frame,
        ),
    };
    Analysed { data, analysis, effects }
}
```

Then, in the body of `elaborate_drops` (lines 109-323):
- Line 109, `subtrees: Vec<Vec<PathId>>`, goes: `data.subtree(p)` replaces it.
- Lines 113-118: `sites`, `writes`, `edge_writes` and `dead_drops` become frame `Vec`s.
- `DropSite.state: InitState<'f>` is still one state per site, but in the frame, and
  `state.clone()` (line 144) is a frame bump.
- `FlagWrite { paths: Vec<PathId>, value }` (line 92) becomes `FlagWrite { path: PathId, value }`.
  At line 155, `subtrees[effect.path].clone()` becomes `effect.path`, and the reader at line 292
  takes `data.subtree(write.path)`.
- Line 128: `let mut state = entry.clone();` becomes one reused state,
  `state.copy_from(entry)`, and `effects_at(..)` (line 130) becomes `effects.at(..)`.
- Lines 160-164, `edge_writes`: `(from, to, Vec<PathId>)` becomes `(from, to, Range<u32>)` into
  one frame `Vec<PathId>` of the edge's paths, which `edge_effects`' callback pushes. The reader
  at 269 expands each path to `data.subtree(path)`, in the same order as the original
  `flat_map`.
- Lines 72 and 178, `flags: FxHashMap<PathId, LocalId>`, becomes
  `flags: &'f mut [Option<LocalId>]` by path index, with `flag_count: usize`:
  - `flags.get(path)` becomes `flags[path]`;
  - `flags.is_empty()` becomes `flag_count == 0`;
  - `elaborator.flag(at)`'s insert becomes `if flags[at].is_none() { flags[at] = Some(new local); flag_count += 1 }`.
  - The locals are made in the same order as before.
  - Line 307, `flags.keys().collect()` then `radix::sort`, becomes
    `flags.iter().enumerate().filter_map(|(p, it)| it.map(|_| p))`: path order, the same as the
    sorted keys. `blaze_ids::radix` is then unused here and its import goes.
- Line 193, `let mut stack = vec![path];` per site: one frame `Vec` reused across sites
  (`stack.clear(); stack.push(path)`).
- Line 202, `resets`: a frame `Vec`.
- Lines 252-264: `set(paths, value) -> Vec<Statement>` becomes
  `set(out: &mut impl Extend<Statement>, paths: &[PathId], value: bool)` plus a
  `count(paths) -> usize` of the paths with a flag. The statements are kept body, so each kept
  container is grown once, to its exact final size:
  - The resets (line 265): `let n = count(data.subtree(path)); let block = &mut body.basic_blocks[block]; block.statements.reserve_exact(n); set(&mut block.statements, ..)`.
  - The edge blocks (lines 269-285): `let mut statements = Vec::with_capacity(n); set(&mut statements, ..)`.
  - The grouped writes (lines 290-305): the `grouped` `Vec<((block, at), Vec<Statement>)>`
    becomes a frame `Vec<((BasicBlockId, usize), u32)>` of (key, end) into one frame
    `Vec<Statement, &Frame>` run, in the same order.
    - Before the splices, each block's total gets one
      `body.basic_blocks[block].statements.reserve_exact(total)`.
    - Then `block.statements.splice(at..at, run.drain(start..end))`; the drain moves the
      statements, and splice doesn't reallocate after the `reserve_exact`.
    - Blocks are spliced in the same reverse order, so the indices still hold.
  - The start block (lines 308-322):
    `let n = count(&flagged) + entry params' counts; let mut start = Vec::with_capacity(n);`,
    then `set` into it.
- Lines 497-534, `drop_ladder`/`drop_halfladder`: `in_cleanup` (`vec!`), `unwind_ladder`,
  `ladder` and `fields` become frame `Vec`s.
- Lines 549-563, `fields()`: it returns a frame `Vec` and gets the frame from `self.frame`
  (a new `frame: &'f Frame` field of `Elaborator`).
- Line 640, `box_free_block`'s fields; lines 712-717, `open_drop_for_enum`'s values, blocks and
  variants; lines 787-826, `open_drop_for_array`'s parts and fields: frame `Vec`s.
  - `SwitchTargets::new(values.iter().copied(), otherwise)` takes an iterator, so a frame slice
    feeds it.
- `vec![borrow]` and `vec![read]` are the statements of new kept blocks, one element each, so
  they stay (`vec![x]` allocates exactly one block).

Budgets:
- `warm_runtime_analyses_allocate_nothing` (§10) holds `analyse` to 0.
- The rewrite allocates only kept-body containers, each grown once. On a body with no drops to
  elaborate, `run_passes_allocates_the_kept_body_only` holds the whole pipeline to the clone
  count.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-drops (2026-10-29), uncompiled: all of §4.7, except that
> `Elaborator` is `<'a, 'c, 'f, 'db>` with `ctx: &'a Ctx<'c, 'db>`, since `Ctx` is invariant in
> its frame (a `RefCell`) and the pass's frame is shorter than the pipeline's.

## 5. Lowering: `src/mir/lower.rs`, `lower/match_tree.rs`, `lower/pattern_matching.rs`

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 5.1 `MirLowerCtx<'a, 'f, 'db>`: the body under construction lives in the frame

Lowering pushes statements one at a time into each block's `Vec`, allocates blocks and locals
one at a time into `Arena`s, and pushes into the body's lists. Every kept container regrows
log(n) times, and the result has spare capacity that `shrink_to_fit` reallocates again. All of
that moves into the frame, and `finish` copies each container once, at its exact size.

New types in `lower.rs`, after `DropScope`:

```rust
/// A block while lowering builds it, in the frame; `finish` copies it into the body.
struct FrameBlock<'f> {
    statements: Vec<Statement, &'f Frame>,
    terminator: Option<Terminator>,
    is_cleanup: bool,
}

/// What lowering builds of a body before it is finished, in the frame.
struct Building<'f, 'db> {
    blocks: Vec<FrameBlock<'f>, &'f Frame>,
    locals: Vec<Local, &'f Frame>,
    /// By binding index: its local.
    binding_locals: Vec<Option<LocalId>, &'f Frame>,
    param_locals: Vec<LocalId, &'f Frame>,
    promoted_locals: Vec<(LocalId, ExprId), &'f Frame>,
    closures: Vec<InternedClosureId<'db>, &'f Frame>,
    unreached: Vec<BasicBlockId, &'f Frame>,
    scope_ends: Vec<LocalId, &'f Frame>,
    panic_unwinds: Vec<(BasicBlockId, usize, BasicBlockId), &'f Frame>,
}
```

`MirLowerCtx::new` (lines 280-318) builds the start block with `basic_blocks.alloc(..)`; it
becomes `building.blocks.push(FrameBlock { statements: Vec::new_in(frame), terminator: None, is_cleanup: false })`,
which is index 0 as before. `drop_scopes: vec![DropScope::default()]` (line 338) becomes a frame
`Vec` holding `DropScope::new_in(frame)`.

`MirLowerCtx` keeps `result: MirBody<'db>` for the scalar fields (`owner`, `start_block`,
`coroutine`, `inherits_overflow_checks`, `phase`, `upvar_locals`). Its containers stay empty
until `finish`. New fields:

```rust
    frame: &'f Frame,
    building: Building<'f, 'db>,
```

Helper methods replace the direct accesses. These are the 9 + 18 + 6 + 4 + 2 + 2 + 2 + 1 + 1
sites `rg 'self\.result\.(basic_blocks|locals|binding_locals|param_locals|...)'` finds:

```rust
fn block(&self, id: BasicBlockId) -> &FrameBlock<'f> {
    &self.building.blocks[id.into_raw().into_u32() as usize]
}
fn block_mut(&mut self, id: BasicBlockId) -> &mut FrameBlock<'f> {
    &mut self.building.blocks[id.into_raw().into_u32() as usize]
}
fn alloc_block(&mut self, statements: impl IntoIterator<Item = Statement>, terminator: Option<Terminator>, is_cleanup: bool) -> BasicBlockId {
    let id = BasicBlockId::from_raw(RawIdx::from_u32(self.building.blocks.len() as u32));
    let mut block = FrameBlock { statements: Vec::new_in(self.frame), terminator, is_cleanup };
    block.statements.extend(statements);
    self.building.blocks.push(block);
    id
}
fn alloc_local(&mut self, local: Local) -> LocalId {
    let id = LocalId::from_raw(RawIdx::from_u32(self.building.locals.len() as u32));
    self.building.locals.push(local);
    id
}
fn local(&self, id: LocalId) -> &Local {
    &self.building.locals[id.into_raw().into_u32() as usize]
}
fn binding_local(&self, binding: BindingId) -> Option<LocalId> {
    self.building.binding_locals.get(binding.into_raw().into_u32() as usize).copied().flatten()
}
fn set_binding_local(&mut self, binding: BindingId, local: LocalId) {
    let at = binding.into_raw().into_u32() as usize;
    if at >= self.building.binding_locals.len() {
        self.building.binding_locals.resize(at + 1, None);
    }
    self.building.binding_locals[at] = Some(local);
}
```

The ids are the same numbers: an `Arena` numbers from 0 in `alloc` order, and so does this.

Sites (anchored by code; lines today):
- 2240 `self.result.basic_blocks.alloc(BasicBlock::default())` becomes
  `self.alloc_block([], None, false)`.
- 2700 and 2704, `self.result.basic_blocks[source].terminator` becomes `self.block_mut(source).terminator` (`self.block(source)` for the read).
- 2999-3005 (`push_statement`) becomes `self.block(block).is_cleanup`,
  `self.block(block).statements.len()`, `self.building.panic_unwinds.push(..)` and
  `self.block_mut(block).statements.push(statement)`.
- 3106 (`unwind_node`) becomes `self.alloc_block(statements, Some(..), true)`. `statements` is
  `[]` or `[StorageDead]`; the `vec![..]` (line 3102) becomes an `Option<Statement>`, which is
  `IntoIterator`.
- 3119 (`resume_block`), 4291 (`drop_assigned`'s unwind block): `alloc_block`.
- 4125 (`exits_end_at`): `let block = self.block_mut(block);`.
- `self.result.locals.alloc(..)` (all sites) becomes `self.alloc_local(..)`, and
  `self.result.locals[l]` becomes `self.local(l)`. `ctx.result.locals` (9 sites in
  `lower_closure_body` and `promoted_mir_body_query`) becomes `ctx.alloc_local` / `ctx.local`.
- `self.result.binding_locals.insert(b, l)` (3717, 3732, 3796) becomes `self.set_binding_local(b, l)`.
- `.get(b)` (3804) becomes `self.binding_local(b)`, and `.contains_idx(b)` (1461, 3738) becomes
  `self.binding_local(b).is_some()`.
- `self.result.param_locals` (3712, 3718, 3723) becomes `self.building.param_locals`.
  - Line 3744, `self.result.param_locals.clone()[base_param_count]`, becomes
    `self.building.param_locals[base_param_count]`. The clone of the whole list was only there
    to index it.
- `promoted_locals` (532, 1582), `closures` (1862, 1864), `scope_ends` (4241, 4252) and
  `pattern_matching.rs:198` `unreached`: `self.building.*`.
- `pattern_matching.rs:910`, `let ty = place.ty(&self.result, &self.infcx, self.env).ty;`. It
  needs only the place's local type (`hir_ty::mir::Place::ty` reads
  `body.locals[self.local].ty`, `hir-ty/src/mir.rs:277-288`), so it becomes
  `let ty = PlaceTy::from_ty(self.local(place.local).ty.as_ref()).multi_projection_ty(&self.infcx, self.env, place.projection.as_slice()).ty;`
  (`multi_projection_ty` is `pub`, line 1854).

`finish` copies each container once, at its final size, into the result:

```rust
impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// The body lowering built, each list of it allocated once at its length.
    fn finish(mut self) -> (MirBody<'db>, InferCtxt<'db>, ParamEnv<'db>, CopyCache<'f, 'db>) {
        let Building {
            blocks, locals, binding_locals, param_locals, promoted_locals, closures, unreached,
            scope_ends, panic_unwinds,
        } = self.building;
        let mut basic_blocks = Arena::with_capacity(blocks.len());
        for FrameBlock { statements, terminator, is_cleanup } in blocks {
            let mut kept = Vec::with_capacity(statements.len());
            kept.extend(statements);
            basic_blocks.alloc(BasicBlock { statements: kept, terminator, is_cleanup });
        }
        let mut kept_locals = Arena::with_capacity(locals.len());
        for local in locals {
            kept_locals.alloc(local);
        }
        let last = binding_locals.iter().rposition(Option::is_some).map_or(0, |it| it + 1);
        let mut bindings = ArenaMap::with_capacity(last);
        for (at, local) in binding_locals.iter().enumerate() {
            if let Some(local) = local {
                bindings.insert(BindingId::from_raw(RawIdx::from_u32(at as u32)), *local);
            }
        }
        let body = &mut self.result;
        body.basic_blocks = basic_blocks;
        body.locals = kept_locals;
        body.binding_locals = bindings;
        body.param_locals = exact(&param_locals);
        body.promoted_locals = exact(&promoted_locals);
        body.closures = exact(&closures);
        body.unreached = exact(&unreached);
        body.scope_ends = exact(&scope_ends);
        body.panic_unwinds = exact(&panic_unwinds);
        (self.result, self.infcx, self.env, self.copy)
    }
}

/// A list of the body, allocated once at its length; nothing when it is empty.
fn exact<T: Copy>(items: &[T]) -> Vec<T> {
    items.to_vec()
}
```

Why it is the same body:
- `BasicBlock`'s fields and order are the same.
- `ArenaMap::insert` resizes its `Vec<Option<V>>` to `idx + 1`, and `with_capacity(last)` holds
  that without regrowing. The resulting `v` has the same length as the original's (one past the
  largest binding inserted; lowering never removes one), so `PartialEq` and `Hash` agree.
- `to_vec()` of an empty slice doesn't allocate, and `Vec::with_capacity(0)` doesn't either.

The callers of `finish`:
- `lower_to_mir_with_store` (line 5051) and `promoted_mir_body_query` (lines 4975-4987) end
  with `let (mut body, infcx, env, mut copy) = ctx.finish(); body.mark_moves(&infcx, env, &mut copy); Ok(body)`.
  - `mark_moves` runs on the finished body, as before. It does
    `mem::take(&mut self.basic_blocks)` and puts it back, which allocates nothing.
  - `ctx.result.shrink_to_fit()` (4986) goes: every container is already exact, so it did
    nothing but cost a pass over the body.
  - `mir_body_query`'s `result.shrink_to_fit()` (4955) goes for the same reason.
- `lower_closure_body` (lines 4607-4861): its block loops (4789, 4807) run on
  `ctx.building.blocks` before `finish`; its `walk_places` (4819) runs on the finished body. The
  `binding_locals` rebuild (4853-4858, a `filter` collect into a new `ArenaMap`) becomes a
  filter in place on `ctx.building.binding_locals` before `finish`
  (`for it in &mut binding_locals { if !keep(*it) { *it = None } }`). The same entries are
  kept, so `finish` builds the same map. Its `ctx.result.shrink_to_fit()` (4860) goes.

Each lowering entry opens the frame:
- `lower_to_mir_with_store` becomes `frame::task(Tag::Mir, |frame| { let mut ctx = MirLowerCtx::new(db, owner, store, infer, frame); .. })`.
- `promoted_mir_body_query` and `lower_closure_body` likewise.
- `MirLowerCtx::new` takes `frame: &'f Frame` and builds every frame field with `new_in(frame)` /
  `framed::map_in(frame)` / `framed::set_in(frame)`.
- What the closure returns (`MirBody<'db>`) is owned, so nothing borrows the frame past the task.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-lower (2026-10-29), uncompiled: `MirLowerCtx<'a, 'f, 'db>` builds in `Building<'f, 'db>` (frame `Vec`s for blocks, locals, `binding_locals` by binding index and the other lists), `finish` copies each into the body at its length, and `lower_closure_body`, `promoted_mir_body_query` and `lower_to_mir_with_store` each run in one `frame::task(Tag::Mir, ..)` ending in `finish` then `mark_moves` without `shrink_to_fit`, with `lower_body(db, def)` being `compute_mir_body` without its span, `copy` an `FxHashMap` until D2, and constants on `from_bytes`/`from_concrete_const` (through one `const_bytes` helper) until D1.

### 5.2 The per-body maps and lists of `MirLowerCtx` (lines 140-224)

| Field | Before | After |
|---|---|---|
| `labeled_loop_blocks` | `FxHashMap<LabelId, LoopBlocks>` | `FrameMap<'f, LabelId, LoopBlocks>` |
| `drop_scopes` | `Vec<DropScope>` | `Vec<DropScope<'f>, &'f Frame>` |
| `extended_temporaries`, `extended_blocks` | `FxHashMap<ExprId, usize>` | `FrameMap<'f, ExprId, usize>` |
| `ends_temporaries`, `drop_temps` | `FxHashSet<ExprId>` | `FrameSet<'f, ExprId>` |
| `undeclared` | `FxHashSet<BindingId>` | `FrameSet<'f, BindingId>` |
| `unwind_nodes` | `FxHashMap<(BasicBlockId, LocalId, bool), BasicBlockId>` | `FrameMap<'f, ..>` |
| `exit_nodes` | `FxHashMap<(BasicBlockId, LocalId, Vec<LocalId>), BasicBlockId>` | `FrameMap<'f, (BasicBlockId, LocalId, &'f [LocalId]), BasicBlockId>` |
| `operand_locals`, `unscheduled` | `FxHashSet<LocalId>` | `FrameSet<'f, LocalId>` |
| `or_choices` | `FxHashMap<PatId, usize>` | `FrameMap<'f, PatId, usize>` |
| `guarded_moves` | `Option<Vec<GuardedMove<'db>>>` | `Option<Vec<GuardedMove<'db>, &'f Frame>>` |
| `guard_refs` | `FxHashMap<BindingId, LocalId>` | `FrameMap<'f, BindingId, LocalId>` |
| `deref_temps` | `FxHashMap<DerefKey, LocalId>` | `FrameMap<'f, DerefKey, LocalId>` |
| `assigned` | `Vec<Assigned>` | `Vec<Assigned, &'f Frame>` |
| `copy` | `FxHashMap<Ty<'db>, bool>` | `CopyCache<'f, 'db>`, see §11 (D2) |

Which maps are iterated:
- `exit_nodes`, `unwind_nodes`, `guard_refs`, `or_choices`, `extended_*` and `deref_temps` are
  only probed and inserted into.
- The sets are only probed.
- `rg 'for .* in .*\.(guard_refs|or_choices|unwind_nodes|exit_nodes)'` finds no iteration that
  reaches the body. The same `FxBuildHasher` keeps any iteration order equal anyway.

`exit_nodes`' key held an owned `Vec<LocalId>` per entry (cloned per probe, at the `exit_to`
call site, line 4042-4091). It becomes a frame slice copied once on insert, and probing borrows:

```rust
/// A key of `exit_nodes` borrowed for a probe.
#[derive(Hash)]
struct ExitKey<'q>(BasicBlockId, LocalId, &'q [LocalId]);

impl<'f> hashbrown::Equivalent<(BasicBlockId, LocalId, &'f [LocalId])> for ExitKey<'_> {
    fn equivalent(&self, key: &(BasicBlockId, LocalId, &'f [LocalId])) -> bool {
        self.0 == key.0 && self.1 == key.1 && self.2 == key.2
    }
}
// probe: self.exit_nodes.get(&ExitKey(after, local, &storage))
// insert: self.exit_nodes.insert((after, local, self.frame.alloc_slice_copy(&storage)), node)
```

`#[derive(Hash)]` on a tuple struct hashes its fields in order, which is what the tuple's `Hash`
does, so the probe finds the entry.

`DropScope` (lines 84-114) becomes `DropScope<'f> { locals: Vec<LocalId, &'f Frame>, temporaries, tail, exits: Vec<BasicBlockId, &'f Frame> }`.
- `#[derive(Default)]` goes. Each `DropScope::default()` site becomes `DropScope::new_in(self.frame)`.
- `Clone` stays derived (`&Frame: Clone`).
- `in_order(&self, operands) -> Cow<[LocalId]>` (lines 105-114) concatenates three `Vec`s per
  call. It becomes an append to a buffer the caller owns:

  ```rust
  fn in_order_into(&self, operands: &FrameSet<'_, LocalId>, out: &mut Vec<LocalId, &Frame>) {
      match self.tail {
          Some(declared) if declared < self.locals.len() => {
              let tail = &self.locals[declared..];
              out.extend(tail.iter().filter(|it| !operands.contains(*it)));
              out.extend_from_slice(&self.locals[..declared]);
              out.extend(tail.iter().filter(|it| operands.contains(*it)));
          }
          _ => out.extend_from_slice(&self.locals),
      }
  }
  ```

  `partition` keeps each side's order, so `[temporaries, before, operands]` is the same
  sequence.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-lower (2026-10-29), uncompiled: as written, with `framed::{FrameMap, FrameSet}`; `exit_nodes` probes with `ExitKey` and copies the key's slice into the frame on insert.

### 5.3 The per-statement unwind path (lines 2997-3043)

`push_statement` calls `unwind_here` for every statement that may panic: every checked
arithmetic op, call and index. `unwind_here` collects every scope's locals through
`leaving_order`, which partitions into two new `Vec`s per scope and then collects a third. The
whole path becomes allocation-free, through one buffer field reused across statements.

New field: `in_scope_buf: Vec<LocalId, &'f Frame>`.

```rust
// before
fn unwind_here(&mut self) -> Option<BasicBlockId> {
    let in_scope: Vec<LocalId> =
        (0..self.drop_scopes.len()).flat_map(|index| self.leaving_order(index)).collect();
    self.unwind_through(&in_scope, None)
}
fn leaving_order(&self, index: usize) -> Vec<LocalId> {
    let operands = &self.operand_locals;
    let (pending, mut rest): (Vec<LocalId>, Vec<LocalId>) =
        self.drop_scopes[index].in_order(operands).iter().partition(|it| operands.contains(it));
    rest.extend(pending);
    rest
}
fn in_scope(&self) -> Vec<LocalId> {
    let operands = &self.operand_locals;
    self.drop_scopes.iter().flat_map(|it| it.in_order(operands).into_owned()).collect()
}
// after
fn unwind_here(&mut self) -> Option<BasicBlockId> {
    let mut in_scope = std::mem::replace(&mut self.in_scope_buf, Vec::new_in(self.frame));
    in_scope.clear();
    for index in 0..self.drop_scopes.len() {
        self.leaving_order_into(index, &mut in_scope);
    }
    let unwind = self.unwind_through(&in_scope, None);
    self.in_scope_buf = in_scope;
    unwind
}
/// Appends the locals of the scope at `index` in the reverse of the order leaving it early
/// drops them in: the scope's order with its pending operands moved to the end.
fn leaving_order_into(&self, index: usize, out: &mut Vec<LocalId, &Frame>) {
    let operands = &self.operand_locals;
    let start = out.len();
    self.drop_scopes[index].in_order_into(operands, out);
    // A stable partition in place: what is not an operand keeps its order at the front, the
    // operands theirs after it.
    let end = out.len();
    let mut kept = start;
    for at in start..end {
        let local = out[at];
        if operands.contains(&local) {
            out.push(local);
        } else {
            out[kept] = local;
            kept += 1;
        }
    }
    out.drain(kept..end);
}
fn in_scope_into(&self, out: &mut Vec<LocalId, &Frame>) {
    for scope in &self.drop_scopes {
        scope.in_order_into(&self.operand_locals, out);
    }
}
```

- `unwind_from_drop` (3036-3043) takes `in_scope_buf` the same way: `clear`, then
  `in_scope_into`, then `truncate` or `extend_from_slice` as before, then `unwind_through`, and
  puts it back.
- `unwind_through` reads `self.local(local).ty`, probes `unwind_nodes` and allocates blocks.
  None of that touches `in_scope_buf`, so taking the buffer out and putting it back has no
  aliasing issue.
- `unwind_node`'s new blocks (3106) are kept body (§5.1).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-lower (2026-10-29), uncompiled: as written; `declare_operands_last`, `end_arguments` and `outlive_block` move operands to the end with one in-place `move_to_end` through a reused `picked_buf`.

### 5.4 Leaving scopes early (`exit_to`, `drop_until_scope`, `exits_end_at`, `emit_drop_and_storage_dead_for_scope`)

- `exit_to` (4042-4091): the `storage` `Vec`, `ended` (4060, a collect per local) and `before`
  (4069) become frame `Vec`s: two reused buffers taken from new fields `exit_storage_buf` and
  `exit_before_buf` and put back as in §5.3. The `exit_nodes` probe borrows them (§5.2).
- `drop_until_scope` (4031) clones a whole `DropScope` per scope it leaves, which is two `Vec`s.
  Instead, copy the scope's `locals` into a reused frame buffer (`in_order_into`), and push the
  exit into `self.drop_scopes[index].exits` by index after the drops are lowered.
- `exits_end_at` (4112-4136): `locals: FxHashSet` becomes a `FrameSet` (only probed), and the
  `exits` clone becomes `std::mem::replace(&mut scope.exits, Vec::new_in(self.frame))` (the
  scope is being popped, so its exits are read once).
- `guard_failed` (4151-4153): its `to_vec`s become frame `Vec`s (`Vec::with_capacity_in` plus
  `extend_from_slice`).
- `emit_drop_and_storage_dead_for_scope` (4301-4328): `in_order(..).into_owned()` becomes
  `in_order_into` into a reused frame buffer. `written: Vec<LocalId>`, returned as `vec![..]`,
  becomes an out-param `&mut Vec<LocalId, &Frame>` that the caller reuses.
- Lines 1175-1224: the `or_choices` collect is into the frame map. The
  `mem::replace(&mut self.guarded_moves, Some(Vec::new()))` becomes `Some(Vec::new_in(self.frame))`.
  The `guard_refs.clone()` at 1188 and 1224 (and `pattern_matching.rs:364`) saves the map around
  the arm, and the save is a frame clone (`FrameMap: Clone`).
- `check_expansions` (4485 collect): a frame `Vec`.
- The rest of the per-expression temporaries listed in the survey become frame `Vec`s
  (`Vec::with_capacity_in(n, self.frame)`, filled as now). Where a list ends up in the body
  (an aggregate's operands, a call's `args: Box<[Operand]>`), it is built straight at its final
  size with `Vec::with_capacity(n)` / `into_boxed_slice()` on an exact `Vec`:
  - 618, 619 and 754: `asm!` operands and templates;
  - 1370, 1417 and 1423: operands `vec![..]`;
  - 1909, 1962, 1997, 2319, 2552 and 2567;
  - 3719 and 3740;
  - 3782: bindings.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-lower (2026-10-29), uncompiled: as written, except that `exits_end_at` reads the popped scope's `exits` in place without the sort and dedup (a block met twice gets the same spans again), `drop_until_scope` and `emit_drop_and_storage_dead_for_scope` share `emit_drops(locals, current, span, written: Option<&mut Vec<BasicBlockId, &Frame>>)`, `exit_to` reuses four buffers, and call arguments, aggregates and closure captures are built at their final size.

### 5.5 `match_tree.rs` and `pattern_matching.rs`

The match tree is built per `match` and dropped once the arms are lowered, so all of it goes in
the frame:
- `Pair { place: Vec<Step>, ask, inside: Vec<Pair> }` becomes
  `Pair<'f> { place: Vec<Step, &'f Frame>, ask: Ask<'f>, inside: Vec<Pair<'f>, &'f Frame> }`.
- `Ask::Or(Vec<Vec<Pair>>)` becomes `Ask::Or(Vec<Vec<Pair<'f>, &'f Frame>, &'f Frame>)`.
- `Candidate { pairs: Vec<Pair>, then: Option<Rc<[Pair]>> }` becomes `Candidate<'f> { pairs: Vec<Pair<'f>, &'f Frame>, then: Option<&'f [Pair<'f>]> }`.
  The `Rc` shared the tail between candidates; a frame slice shares it the same way (`Pair` has
  no `Drop` that matters: it holds only frame `Vec`s and `Copy` data).
- `Tree { reached: Vec<bool>, escaped }` becomes `Tree<'f> { reached: Bits<'f>, escaped }`, and
  `reached_arms() -> Vec<bool>` returns `&Bits<'f>`. Its one reader in `pattern_matching.rs`
  does `reached[arm]`, which becomes `reached.contains(arm)`.
- Allocation sites: 114-115, 129, 137, 157-167, 268-276, 312-322, 353-406, 443 and 493 become
  `Vec::new_in(frame)` / `Vec::with_capacity_in(n, frame)`. The frame comes from
  `MirLowerCtx::frame`, and the tree builder takes `frame: &'f Frame`.
- `pattern_matching.rs` 143, 151, 215-228 (`expansions: vec![Vec::new()]` becomes a frame `Vec`
  of frame `Vec`s), 364 (above), 1022 and 1076: frame `Vec`s.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-lower (2026-10-29), uncompiled: frame `Pair<'t>`, `Ask<'t>` and `Candidate<'t>`, except that `then` is an index into the tree's own `thens` list instead of a leaked slice, `Ask::Equal` borrows the store's `&Literal` with a negated flag instead of cloning it, `test` takes the first candidate's pair by value instead of cloning it, and `reached_arms` returns a frame `Bits<'f>` by value, read with `contains`.

### 5.6 `mir_body_query`'s tracing detail (lines 4920-4941)

Every lowering formats the body's name into a `String` for a tracing span, whether or not
anything records it:

```rust
// before
let detail = match def { .. .display(db, edition).to_string() .. };
let _p = tracing::info_span!("mir_body_query", ?detail).entered();
// after
let span = tracing::info_span!("mir_body_query", detail = tracing::field::Empty);
if !span.is_disabled() {
    // alloc: only while a subscriber records this span.
    let detail = match def { .. unchanged .. };
    span.record("detail", tracing::field::debug(&detail));
}
let _p = span.entered();
```

`field::debug(&String)` records what `?detail` recorded. The span is the same, and the field is
only filled in when the span is enabled.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 5.7 `lower_body_to_mir` (line 5013)

`let resolver = owner.resolver(db); let interner = DbInterner::new_with(db, resolver.krate());`
builds a whole `Resolver` (a `Vec` of scopes) only to read its crate. It becomes
`let interner = DbInterner::new_with(db, owner.krate(db));`. It is the same crate: the
resolver's module is the owner's, and `transform::Ctx::new` already does this. That is one
`Resolver` fewer per body.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

## 6. The optimizations: `src/mir/optimize.rs` and `optimize/*`

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 6.1 `optimize` (lines 89-158): one task per pass

```rust
// before
    let mut body = body.clone();
    ..
    let ctx = Ctx::new(db, body.owner);
    macro_rules! run {
        ($pass:literal, $($run:tt)*) => {{
            $($run)*;
            #[cfg(debug_assertions)]
            if let Err(error) = validate(&body) { panic!(..) }
        }};
    }
    if optimizing { run!("CleanupPostBorrowck", cfg::cleanup_post_borrowck(&mut body)); .. }
    ..
    Ok(body)
// after
    // The copy the query keeps: the passes change it in place.
    let mut body = body.clone();
    ..
    frame::task(Tag::Mir, |outer| passes(&Ctx::new(db, body.owner, outer), &mut body, options, optimizing))?;
    Ok(body)
}

fn passes<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    options: MirOptions,
    optimizing: bool,
) -> Result<'db, ()> {
    let release = options.level >= MirOptLevel::Release;
    // Each pass in a frame of its own, and in a debug build a check that it left a body the
    // next can read.
    macro_rules! run {
        ($pass:literal, |$frame:ident| $run:expr) => {{
            frame::task(Tag::Mir, |$frame| -> Result<'db, ()> {
                $run;
                #[cfg(debug_assertions)]
                if let Err(error) = validate(body, $frame) {
                    panic!("MIR of {:?} after {}: {error}", body.owner, $pass);
                }
                Ok(())
            })?
        }};
    }
    if optimizing {
        run!("CleanupPostBorrowck", |frame| cfg::cleanup_post_borrowck(body, frame));
        run!("SimplifyCfg", |frame| cfg::simplify_cfg(body, frame));
    }
    // .. every other run! of lines 115-156 the same way, `&mut body` becoming `body` and the
    // pass taking `frame` last; `return Ok(body)` (line 125) becoming `return Ok(())`.
    Ok(())
}
```

Pass signatures that gain `frame: &Frame` (last parameter):
- `cfg::{cleanup_post_borrowck, simplify_cfg, simplify_const_condition, remove_unneeded_drops, critical_call_edges, unreachable_propagation}`;
- `locals::{remove_storage_markers, simplify_locals, remove_unused_definitions}`;
- `dse::dead_store_elimination`;
- `copy_prop::copy_prop`;
- `inst_simplify::{inst_simplify, lower_slice_len_calls, single_use_consts}`;
- `check_pointers::check_pointers`;
- `check_enums::check_enums`;
- `transform::remove_noop_landing_pads`.

A pass that doesn't use it yet still takes it, so the macro is uniform. The early return at
line 98 (`if !optimizing && !options.ub_checks { return Ok(body) }`) stays before the task, with
the clone, as now.

Add, as `transform::run_one` (§3.2):

```rust
pub(super) fn run_one<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    pass: super::budget::Pass,
    frame: &Frame,
) -> Result<'db, ()> {
    use super::budget::Pass;
    use check_pointers::PointerCheck;
    match pass {
        Pass::CleanupPostBorrowck => cfg::cleanup_post_borrowck(body, frame),
        Pass::SimplifyCfg => cfg::simplify_cfg(body, frame),
        Pass::RemoveStorageMarkers => locals::remove_storage_markers(body, frame),
        Pass::RemoveUnneededDrops => cfg::remove_unneeded_drops(ctx, body, frame),
        Pass::UnreachablePropagation => cfg::unreachable_propagation(body, frame),
        Pass::InstSimplify => inst_simplify::inst_simplify(ctx, body, frame),
        Pass::LowerSliceLenCalls => inst_simplify::lower_slice_len_calls(ctx, body, frame),
        Pass::SimplifyConstCondition => cfg::simplify_const_condition(body, frame),
        Pass::SimplifyLocals => locals::simplify_locals(body, frame),
        Pass::SingleUseConsts => inst_simplify::single_use_consts(body, frame),
        Pass::DeadStoreElimination => dse::dead_store_elimination(body, frame),
        Pass::CopyProp => copy_prop::copy_prop(ctx, body, true, frame),
        Pass::CriticalCallEdges => cfg::critical_call_edges(body, frame),
        Pass::CheckAlignment => check_pointers::check_pointers(ctx, body, PointerCheck::Alignment, frame)?,
        Pass::CheckNull => check_pointers::check_pointers(ctx, body, PointerCheck::Null, frame)?,
        Pass::CheckEnums => check_enums::check_enums(ctx, body, frame)?,
        other => return super::transform::run_one(ctx, body, other, frame),
    }
    Ok(())
}
```

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 6.2 The shared helpers in `optimize.rs`

- **`validate`** (lines 360-407, debug builds only) takes `frame`. The
  `FxHashSet<BasicBlockId>` collected from `reverse_postorder` becomes a `Bits` over block
  indices. The `format!`s are the error path: `// alloc: only when a pass broke the body, which panics`.
- **`reverse_postorder(body) -> Vec<BasicBlockId>`** (lines 530-560) becomes
  `reverse_postorder<'f>(body, frame: &'f Frame) -> Vec<BasicBlockId, &'f Frame>`: `visited` is
  a `Bits`, and `post` and `stack` are frame `Vec`s (`Vec::with_capacity_in(count, frame)`).
  - The stack's `SmallVec<[BasicBlockId; 4]>` of successors stays; it spills only for a switch
    with more than 4 targets. With a frame, store `(block, next index)` and read successors from
    the terminator again instead, so nothing spills. That is the same order.
  - Callers: `Dominators::new`, `validate`, `cfg::unreachable_propagation`, `dse`, `copy_prop`.
- **`Dominators`** (lines 563-640) becomes `Dominators<'f> { idom: Vec<Option<usize>, &'f Frame> }`
  with `new(body, frame)`:
  - `order` becomes a frame `Vec`.
  - `preds: Vec<Vec<usize>>`, one heap `Vec` per block, becomes a CSR in the frame: count the
    predecessors of each block, prefix-sum into `starts`, then fill `all`. Each block's
    predecessors are in the same order: `rpo` order, then successor order.
  - `idom` becomes a frame `Vec`.
- **`map_place`** (lines 801-815) collects the new projection into a `Vec<PlaceElem>` and then
  interns it. Pass the iterator straight to `Projection::new_from_iter`: it collects into
  `rustc_type_ir::CollectAndApply`'s `SmallVec<[_; 8]>`, inline for every place of 8 elements or
  fewer. That is the same interned projection.

  ```rust
  // before
  let elems: Vec<PlaceElem> = place.projection.as_slice().iter().map(..).collect();
  place.projection = super::Projection::new_from_iter(elems).store();
  // after
  let elems = place.projection.as_slice().iter().map(|it| match *it {
      ProjectionElem::Index(local) => ProjectionElem::Index(map(local)),
      other => other,
  });
  place.projection = super::Projection::new_from_iter(elems).store();
  ```
- **`renumber_locals`** (lines 820-867) rebuilds every list of the body. Each becomes an
  in-place filter that keeps the order, so nothing is reallocated (the kept `Vec`s only shrink
  in length):
  - `body.param_locals`, `promoted_locals`, `promoted_constants`, `scope_ends` and
    `upvars_in_place`: `.retain_mut(|it| match get(*it) { Some(new) => { *it = new; true } None => false })`.
    For the pairs it is `it.0`.
  - `binding_locals`: renumber in place with `iter_mut` when every binding's local is kept,
    which is always the case from `simplify_locals`: binding locals are always used
    (`locals.rs:36`). If one is not kept, rebuild as now into `ArenaMap::with_capacity(last + 1)`,
    where `last` is the largest kept binding.
    - Removing in place isn't an option: `ArenaMap::remove` leaves the slot as `None`, and the
      map's derived `PartialEq`/`Hash` compare its `Vec` with its trailing `None`s, so the body
      would differ from today's when the largest binding went.
  - `upvar_locals` (`FxHashMap<BindingId, Vec<(LocalId, Place)>>`, kept): `retain` on the map,
    with an inner `retain_mut` on each `Vec` that renumbers, and drops the entry when its list
    ends up empty. That is the same as the rebuild, including the removal of empty lists.
    `HashMap` equality ignores order.
  - `body.locals = new_locals;` stays: the new arena is built by the caller (§6.4).
- **`renumber_blocks(body, blocks: Vec<BasicBlock>, map)`** (lines 870-891) becomes
  `renumber_blocks(body, map: &[Option<usize>])`:
  - The caller has already put the kept blocks in `body.basic_blocks`: the same arena when
    nothing was removed, or a new `Arena::with_capacity(kept)` (kept body, at its exact size),
    with the blocks moved in, not cloned (§6.3).
  - It rewrites the targets in place (`successors_mut` on each terminator) and remaps
    `start_block`.
  - `la_arena::Arena` has no `truncate` or `retain`, so compacting in place isn't possible;
    one exact arena when something went is the least the body allows.
  - `unreached` and `panic_unwinds` become `retain_mut` in place, as above.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir (2026-10-29), uncompiled, §6.1 and §6.2 in `optimize.rs`:
> - `optimize` makes the copy and runs `passes` in one `frame::task`. Each `run!` is a
>   `frame::task` of its own, with `validate(body, frame)` in debug builds. `optimize::run_one`
>   is as written, and its `other` arm goes to `transform::run_one`.
> - `validate`'s reachable set is a `Bits`. `reverse_postorder(body, frame)` keeps
>   `(block, next index)` on a frame stack sized to the block count.
> - `Dominators<'f>` keeps its predecessors in a frame CSR (`starts` and `preds`), in reverse
>   postorder and then successor order, as before.
> - `map_place` passes the iterator straight to `Projection::new_from_iter`.
> - `renumber_locals` filters every list in place with `retain_mut`. `binding_locals` is
>   renumbered in place when every binding is kept; otherwise it is made again at
>   `last + 1`, as the trailing-`None` note says.
> - `renumber_blocks(body, map)` takes the arena the caller filled, rewrites its targets in
>   place, and filters `unreached`/`panic_unwinds` with `retain_mut`. cfg.rs's caller
>   (`renumber_blocks(body, kept, &map)`, §6.3) moves to the new signature.
> - One step past the text: `successors(kind)` is now a non-allocating double-ended iterator
>   (a slice of targets chained with two `Option`s), in the order the `SmallVec` had. So no
>   switch spills anywhere, and the index-based walk is `.nth(at)`. Every caller compiles
>   unchanged: `for`, `.into_iter()`, `.rev()`, `.extend(..)`, `.find(..)`.
> - `passes` and `run_one` call every pass with `frame` last, as listed (for
>   codegen-mir-store's §6.3-6.8), and `Ctx<'_, 'db>`.

### 6.3 `optimize/cfg.rs`

- **`simplify_cfg`** (lines 53-59) moves every block out of the arena into a fresh `Vec`
  (`collect`), then rebuilds an arena in `remove_dead_blocks`, every time, even when nothing
  changes. It becomes:

  ```rust
  pub(super) fn simplify_cfg(body: &mut MirBody<'_>, frame: &Frame) {
      let start = block_index(body.start_block);
      CfgSimplifier::new(body.basic_blocks.as_mut(), start, frame).simplify();
      remove_dead_blocks(body, start, frame);
  }
  ```

  `Arena<T>: AsMut<[T]>`, and `CfgSimplifier` already works on `&mut [BasicBlock]`.
- **`CfgSimplifier`**: `pred_count: Vec<u32>` (line 68, `vec!`) becomes `&'f mut [u32]`
  (`frame.alloc_slice_fill_with(len, |_| 0)`), and `merged` (line 84) becomes a frame `Vec`.
- **`simplify`** (line 103): `let statements = std::mem::take(&mut self.blocks[from].statements); self.blocks[bb].statements.extend(statements);`
  stays. It moves kept statements into the surviving block, which may grow its kept `Vec`
  (amortized) and frees the merged one.
- **`collapse_goto_chain`** (line 128) makes a new `terminators` `Vec` per successor per
  iteration. It becomes a field `chain: Vec<(usize, Terminator), &'f Frame>` of the simplifier,
  cleared on entry: `let mut terminators = std::mem::replace(&mut self.chain, Vec::new_in(self.frame));`
  ... `self.chain = terminators;`.
- **`reachable(blocks, start) -> Vec<usize>`** (lines 182-200) becomes
  `reachable_into(blocks, start, out: &mut Vec<usize, &Frame>, frame)`, with `seen` as a `Bits`
  and `stack` a frame `Vec`.
- **`remove_dead_blocks(body, start, frame)`** (lines 204-235):
  - `keep` becomes a `Bits`, and `map` a frame slice.
  - If every block is kept and no empty `unreachable` block repeats, `map` is the identity, and
    it returns without touching the arena (the warm case).
  - Otherwise it moves the kept blocks into `Arena::with_capacity(kept_count)`; `kept_count` is
    computed in the first loop. Then it calls `renumber_blocks(body, &map)`, which fixes the
    targets in place.
- **`simplify_const_condition`** (lines 264-298): `Some((place.clone(), value.clone()))`
  (line 269) clones a `StoredPlace` (a refcount) and an `Operand`. An `Operand` is an
  interned handle for `Allocation`/`Constant`, so its clone allocates nothing either. Borrow
  instead, `Some((place, value))` as references, because `block.statements` isn't changed
  while `before` is alive: the terminator is a different field. That gives
  `let before = block.statements.last().and_then(..)`, then
  `let Some(terminator) = &mut block.terminator`, which are disjoint field borrows. Zero
  allocations either way; this saves the refcount traffic.
- **`remove_unneeded_drops`**: in place, unchanged. Its `simplify_cfg(body)` call becomes
  `simplify_cfg(body, frame)`.
- **`critical_call_edges`** (lines 322-365): `pred_count` (`vec!`) becomes a frame slice, and
  the `work` collect a frame `Vec`. The guard blocks it allocates are kept body.
- **`unreachable_propagation`** (lines 371-419):
  - `FxHashSet<BasicBlockId>` becomes a `Bits` over block indices, and `order` comes from the
    frame `reverse_postorder`.
  - `kept: Vec<(u128, BasicBlockId)>` per switch becomes one frame `Vec` reused across
    switches.
  - The final `for bb in unreachable` (it iterated a hash set, but clearing is
    order-independent) becomes `for bb in unreachable.iter()`.
  - `SwitchTargets::new(kept.iter().copied(), otherwise)` builds the kept terminator, so it
    stays.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written, every pass taking `frame` last, with `framed::{Bits, bits_in, block_index, block_id}`; `remove_dead_blocks` returns before touching the arena when every block is kept (`kept == count`), and otherwise moves the kept blocks into `Arena::with_capacity(kept)` and calls `renumber_blocks(body, map)`.

### 6.4 `optimize/locals.rs`

- `UsedLocals { count: Vec<u32>, always: Vec<bool> }` (lines 27-48) becomes
  `UsedLocals<'f> { count: &'f mut [u32], always: Bits<'f> }`, with `new(body, frame)`.
- `remove_unused_definitions_helper` (line 100): `let blocks: Vec<_> = body.basic_blocks.iter().map(|(id, _)| id).collect();`
  per round becomes `for index in 0..body.basic_blocks.len() { let block = block_id(index); .. }`.
  The loop never adds blocks.
- **`simplify_locals`** (lines 134-151) builds `map` and a new `Arena` with `decl.clone()` of
  every local before it knows whether anything was removed. So every warm run allocates two
  blocks and throws them away. It becomes:

  ```rust
  pub(super) fn simplify_locals(body: &mut MirBody<'_>, frame: &Frame) {
      let mut used = UsedLocals::new(body, frame);
      remove_unused_definitions_helper(&mut used, body);
      let kept = body.locals.iter().filter(|(local, _)| used.is_used(*local)).count();
      if kept == body.locals.len() {
          return;
      }
      let map = frame.alloc_slice_fill_with(body.locals.len(), |_| None);
      let mut new_locals = Arena::with_capacity(kept);
      for (local, decl) in std::mem::take(&mut body.locals).into_iter() {
          if used.is_used(local) {
              map[local_index(local)] = Some(new_locals.alloc(decl));
          }
      }
      renumber_locals(body, new_locals, map);
  }
  ```

  The new arena is kept body at exact size, and its locals are moved, not cloned. The
  numbering is the same: kept locals in old order.
  - `renumber_locals` uses `map` only for lookups, so `&[Option<LocalId>]` from a frame slice
    is fine.
  - `la_arena::Arena: IntoIterator<Item = (Idx<T>, T)>` yields in index order.
- **`remove_storage_markers`**: in place, unchanged (gains the `frame` parameter only).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written; `remove_unused_definitions` (copy_prop's) takes `frame` too, and `simplify_locals` passes `renumber_locals` the frame slice `map`.

### 6.5 `optimize/dse.rs`

- `struct Bits(Vec<u64>)` (lines 21-42) goes; it uses `framed::Bits<'f>`.
- `statement_effect` and `terminator_effect` (lines 73-107) allocate a `mentions` `Vec` per
  statement per fixpoint round. Each takes `mentions: &mut Vec<(LocalId, Mention), &Frame>`,
  cleared on entry; the caller makes one before the fixpoint.
- `entry: Vec<Bits>` (line 150, `vec![Bits::new(locals); blocks]`) becomes a frame `Vec` of
  frame `Bits`.
- `exit_of` (lines 151-159) returns a new `Bits` per block per round. It becomes
  `exit_into(entry, block, live: &mut Bits<'_>)`: `live.clear()`, then a union over each
  successor.
- Line 165: `let mut live = exit_of(..)` becomes one reused `live`.
- Line 178: `if live != entry[..] { entry[..] = live; changed = true }` becomes
  `if live != entry[..] { entry[..].copy_from(&live); changed = true }`.
- `dead`, `to_move` (185-186) and the per-call `state = live.clone()` (194) become frame `Vec`s
  and a reused `state` (`state.copy_from(&live)`).
- `remove_unused_storage_markers` (line 250): `used` becomes a `Bits` in the frame.
- `dead_store_elimination(body)` becomes `dead_store_elimination(body, frame)`, and
  `remove_unused_storage_markers(body)` becomes `(body, frame)`.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written, on `framed::Bits` (`blaze_ids::bits` over frame words, which also does `v2/requests/codegen.md`'s dedup item 2), with `exit_into` a function and one `mentions` frame `Vec` for the whole pass.

### 6.6 `optimize/copy_prop.rs`

- `Ssa { assignments: Vec<Assignments>, order: Vec<LocalId>, borrowed: Vec<bool> }` (lines
  72-77) becomes `Ssa<'f>` with frame `Vec`s and `borrowed: Bits<'f>`.
- In `ssa_locals(ctx, body, frame)` (lines 79-160):
  - `preds` (`vec!`) becomes a frame slice.
  - `single_pred` (a collect) becomes a `Bits`.
  - `Dominators::new(body, frame)` and `reverse_postorder(body, frame)` are the frame versions.
  - `check_dominates`' parameter becomes `&mut Vec<Assignments, &Frame>`.
- `copy_classes(ssa, body) -> Vec<LocalId>` (lines 163-204) becomes `-> Vec<LocalId, &'f Frame>`.
- `unified: FxHashSet<LocalId>` (line 213) becomes a `Bits` over local indices. It is only
  probed; `unified.is_empty()` becomes `Bits::is_empty`.
- Lines 254-258 rebuild `binding_locals` into a fresh `ArenaMap` with every entry renumbered.
  Nothing is removed, so it becomes
  `for (_, local) in body.binding_locals.iter_mut() { *local = lookup(*local); }`. The same
  entries in the same slots, so the same `v`.
- `copy_prop(ctx, body, lifetimes)` becomes `copy_prop(ctx, body, lifetimes, frame)`.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written, except that `check_dominates` takes `&mut [Assignments]` (a slice of the frame `Vec`) and `order` is made `with_capacity_in(count)`, since a local enters it at most once.

### 6.7 `optimize/inst_simplify.rs`

- `single_use_consts(body)` (lines 238-300) becomes `single_use_consts(body, frame)`:
  `ineligible`, `init`, `uses`, `whole_reads` and `mentions` (the `vec!`s per local at lines
  240-277) become frame slices or `Bits`.
- `inst_simplify` (line 72) does `std::mem::take(&mut body.basic_blocks)` and puts it back. That
  allocates nothing; unchanged.
- `lower_slice_len_calls`: in place; unchanged.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written; `ineligible` is a `Bits`, the other four are frame slices, and `inst_simplify`/`lower_slice_len_calls` take an unused `frame` for the uniform `run!`.

### 6.8 `optimize/check_pointers.rs`, `optimize/check_enums.rs`

- `check_pointers.rs:338` and `check_enums.rs:55`: `let blocks: Vec<_> = body.basic_blocks.iter().map(|(id, _)| id).collect();`
  becomes `let blocks = body.basic_blocks.len(); for index in 0..blocks { let id = block_id(index); .. }`.
  New blocks are appended after `blocks`, as with `ids`.
- `check_pointers.rs:362`: the `setup` `Vec` per site becomes a frame `Vec` reused across
  sites. Its statements are then moved into the kept block they belong to, and that block's
  `Vec` is made `with_capacity` its exact length.
- `check_pointers.rs:138`: `sites()` returns `SmallVec<[_; 8]>`, inline for 8 or fewer sites
  per statement. Unchanged.
- `check_enums.rs:44,148`: `valid: Vec<u128>` becomes a frame `Vec`. Line 232's `setup` gets
  the same treatment as `check_pointers`' `setup`.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store-opt (2026-10-29), uncompiled: as written, one frame `Setup` per pass moved in by `check_pointers::append` (`reserve_exact` then `drain`), with `check_enums`' trailing statements going through it too; beyond the text, `sites()` returns its iterator (`+ use<>`, no borrow of the body) instead of a `SmallVec` that spilled past 8 statements, and `transmute_at` borrows its operand so only `find` copies it.

## 7. The inliner (`optimize/inline.rs`, `optimize/inline/*`): not compiled yet

`optimize.rs` doesn't declare `mod inline;` yet; `codegen-mir` is registering it (it reads
`body.inlined`). When it is registered, the same rules apply, and here is the list so it lands
on the frame from the start:
- **`inline.rs`, `Inliner::run` (line 250):** `history` (`Vec`), `listed` (`FxHashMap`) and
  `stack` (`vec!`) become a frame `Vec`, a `FrameMap` and a frame `Vec`. The `arg_tys` and
  `nested` `SmallVec`s stay.
- **`inline/integrate.rs`:**
  - `Temps.before` and `Temps.made`, `params`, `marked` (`vec!`), `map` and `always_live` become
    frame `Vec`s, `Bits` or slices;
  - `Spans.own` (`FxHashMap`) becomes a `FrameMap`;
  - the `scopes`, `bindings`, `promoted` and `ends` collects (lines 155-338) become frame
    `Vec`s;
  - `args.into_vec()` moves the call's kept `Box<[Operand]>` into the callee's locals, so it
    stays.
- **`inline/cost.rs`:** `visited` and `work` (lines 162-163) become `Bits` and a frame `Vec`.
- **`inline/cycle.rs`:** `callees_of`'s `seen` and `callees`, and the walk's `seen`,
  `recursion`, `graph` and frame stack (lines 222-226), become frame values. The `Callgraph`'s
  sorted lists, if memoized, are owned results built at exact size.
- The callee body cloned per call site (`monomorphized`) becomes the inlined blocks, so it is
  kept body.

Budget: `Pass::Inline` joins `budget::OPTIMIZATIONS` when it is registered, and the warm test
(`warm_optimizations_allocate_nothing`) then covers it.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> codegen-mir (2026-10-29): not applied yet. The inliner is still not declared (`mod inline;`)
> because its registration waits on db's `Inlined`/`InlinedBody`/`MirOptions::inline`/
> `MirSpan::Inlined` (`v2/requests/codegen-inline.md` 1-2). These frame moves land with it.
> Its uses of `successors`, `map_place` and `successors_mut` already compile against
> §6.2's helpers.

## 8. Monomorphization: `src/mir/monomorphization.rs`

- `Filler<'db>.universes: Vec<Option<UniverseIndex>>` (line 42) becomes
  `Vec<Option<UniverseIndex>, &'f Frame>`, so `Filler<'f, 'db>`, and `Filler::new` takes
  `frame`.
- `monomorphized` (lines 375-385):

  ```rust
  // after
  pub fn monomorphized<'db>(db, body, subst, trait_env) -> Result<MirBody<'db>, MirLowerError<'db>> {
      // The copy the query keeps: the fold changes it in place.
      let mut body = body.clone();
      frame::task(Tag::Mir, |frame| {
          let mut filler = Filler::new(db, trait_env.as_ref(db), subst.as_ref(), frame);
          filler.fill_body(&mut body)
      })?;
      Ok(body)
  }
  ```
- `fill_operand`'s `Allocation` arm (lines 239-250) clones `memory` and `memory_map` and
  interns a new `Allocation` even when filling leaves the type unchanged. That is one `Box<[u8]>`
  (plus the map's) per constant operand per instance, freed again when the interner finds it.
  Fill the type first and keep the handle when it comes back equal:

  ```rust
  // after, in the Allocation arm
  let ty = allocation.as_ref().ty;
  let filled = self.fill_ty(ty)?;
  if filled == ty {
      return Ok(());
  }
  // .. the existing rebuild with `filled`, unchanged ..
  ```

  `Allocation` handles are equal exactly when their data is, so keeping the handle gives the
  same body (`next_solver/allocation.rs`: a `table::Values` row).
- `opaque_of_impl`'s `vars` collect (line 446) is cold (`// alloc: once per opaque type of a trait method, not per body`).

Budget: `monomorphize_allocates_the_kept_body_only` (§10).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: the `Allocation` arm keeps the handle when
> the type fills to itself, and `opaque_of_impl`'s `vars` carries its `// alloc:`. Not applied:
> `universes` in the frame, because the solver's `BoundVarReplacer::replace_bound_vars` takes
> `&mut Vec<Option<UniverseIndex>>` on the global allocator (an empty `Vec` allocates nothing
> until a binder is entered, so this costs nothing for most bodies); with that the frame task
> around `monomorphized` would hold nothing and is left out. Test:
> `tests/mir/alloc_budget.rs` `monomorphize_allocates_the_kept_body_only`, through the public
> `monomorphized` (no `budget` hook): a body with 1 and one with 12 constants allocate the same
> beyond their copy.

## 9. Per-call paths outside the pipelines

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 9.1 The interpreter, `src/mir/eval.rs`

An `Evaluator` runs one evaluation. Its frames live on `code_stack` across thread switches and
calls, so a `frame::task` per call doesn't fit: a call returns to the interpreter loop, not to
its caller. Its per-call work is made allocation-free by reuse instead:
- **`IntervalOrOwned`** (line 530) is built for every arithmetic result, compare and cast:
  `Owned(Vec<u8>)` and `Shadowed(Vec<u8>, S)` are a heap block per operation. The bytes become
  `SmallVec<[u8; 16]>` (`type OwnedBytes = SmallVec<[u8; 16]>;`). 16 bytes holds every scalar
  up to `u128` and a wide pointer, and it spills only for SIMD and aggregates. A
  `SmallVec<[u8; 16]>` is 24 bytes, the size of the `Vec` it replaces.
  - Sites: every construction of the two variants. They are mostly written unqualified
    (`Owned(..)`, the variants being imported), so the count is
    `rg -c '\bOwned\(|Shadowed\(' eval.rs` = 53 in `eval.rs`, plus `macro_call.rs:53` and
    `shim.rs:2093, 2142`. The per-operation ones:
    - `binary_op`: 1843 (`c.to_vec()`), 1896, 1902, 1925, 1959, 1991, 2023, 2049
      (`vec![r]`/`vec![0]`), 2142;
    - casts: 2212, 2261, 2296, 2325, 2360-2397;
    - the checked and wrapping ops: 2624-2627, 2696 and 2742 (`vec![0; size]`), 2774-2778;
    - 3179 (`Cow::Owned(pad16(..).to_vec())`), and 3432-3473.
  - Their producers: `bytes.to_vec()` becomes `OwnedBytes::from_slice(bytes)`,
    `vec![r]` becomes `smallvec![r]`, `vec![0; n]` becomes `smallvec![0; n]`, and
    `x.to_le_bytes()[..n].to_vec()` becomes `OwnedBytes::from_slice(&x.to_le_bytes()[..n])`.
    Line 3179's `Cow<[u8]>` becomes `OwnedBytes` too, or stays a borrow where `pad16` returns
    the input unchanged.
  - The consumer at 1087 (`Owned(bytes) => Ok(bytes)`) returns `OwnedBytes`; its callers use the
    bytes as `&[u8]` (`write_memory(addr, &r)`, 3472), which `SmallVec` derefs to.
- **`exec_call`'s `args_evaluated`** (lines 1365-1368, a `collect` into a `Vec` per call). The
  callees take `&[IntervalAndTy]` (lines 1382, 1393, 1405), so a reused buffer works. A shim
  may run a nested `interpret_mir` that calls again, so the buffer comes from a pool, not a
  single field:

  ```rust
  // new Evaluator field
  spare_args: Vec<Vec<IntervalAndTy<'db>>>,
  // in exec_call
  let mut args_evaluated = self.spare_args.pop().unwrap_or_default();
  args_evaluated.clear();
  for it in args.iter() {
      match self.operand_ty_and_eval(it, locals) {
          Ok(it) => args_evaluated.push(it),
          Err(error) => {
              self.spare_args.push(args_evaluated);
              return Err(error);
          }
      }
  }
  let ran = (|| { .. the rest of exec_call, using &args_evaluated .. })();
  self.spare_args.push(args_evaluated);
  ran
  ```

  Errors are returned in the same order; the pool only changes where the `Vec` comes from.
- **The `CHECKS` path's `ids`** (line 1773:
  `let ids: Vec<LocalId> = locals.ptr.iter().map(|(id, _)| id).collect();`) exists because
  `set_storage` borrows `locals` mutably. It becomes
  `for (id, _) in body.locals.iter() { if locals.ptr.get(id).is_none() { continue } .. }`.
  `body` is borrowed separately from `locals`, and `ArenaMap::iter` and `Arena::iter` both go
  in index order.
- **`interpret_mir`'s `code_stack`** (line 1175:
  `let prev_code_stack = mem::take(&mut self.code_stack);`). The nested run then grows a fresh
  `Vec` and drops it at 1186. It gets a pool too:
  `let prev_code_stack = mem::replace(&mut self.code_stack, self.spare_code_stacks.pop().unwrap_or_default());`,
  and at 1186
  `let mut my_code_stack = mem::replace(&mut self.code_stack, prev_code_stack); .. my_code_stack.clear(); self.spare_code_stacks.push(my_code_stack);`
  after its last use.
- **`StackFrame.drops: Vec<DropStep>`.** `DropStep::Glue`'s metadata `Vec<u8>` becomes
  `OwnedBytes`. Frames are reused through `unused_locals_store`, which already exists (lines
  1439 and 1710); the frame's `drops` `Vec` is kept with it (`clear()`, not a new `Vec`).
- **The pub `interpret_mir`** (lines 680-705) clones `read_statics` per loop round. It becomes
  one `Vec` reused (`clear` + `extend`).
- `storage_marked` (lines 1627-1643) is an `Arc<FxHashSet>` per body, built once per evaluation
  and cached by the body's address; unchanged
  (`// alloc: once per body an evaluation runs, reused for each of its calls`).
- `Evaluator::new` (lines 829-873, `vec![0]` stack and heap, the empty `Vec`s) and the error
  paths (1196-1202's `error_stack`, the `MirEvalError` boxes at 3080-3133, and the 50 string
  sites the hot lint lists) are one evaluation's setup and its errors; §12 lists them.

Budget: `warm_interpreter_calls_allocate_nothing` (§10).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir (2026-10-29), uncompiled, in `eval.rs` and `eval/`:
> - `type OwnedBytes = SmallVec<[u8; 16]>` holds `Owned` and `Shadowed`. Every producer builds
>   it in place: `from_slice`, `smallvec!`, `IntValue::to_bytes` and `convert_apfloat`
>   return it, and `with_metadata`, `construct_shadowed` and `pointer_value` build one. The
>   `Cow` at the constant write is now a `&[u8]` with a padded local. The shims' `Location`
>   fields and the `with_overflow` intrinsic build `OwnedBytes`. `reflection.rs`'s and
>   `macro_call.rs`'s setup-path `Vec`s go in through `from_vec`.
> - `exec_call` takes its argument buffer from `spare_args` and gives it back on every path
>   (the rest runs in a closure). The error from an argument comes back as before.
> - The `CHECKS` frame-entry walk goes over `body.locals` with `locals.ptr.get`, and
>   `frame_start` is computed once.
> - `interpret_mir` takes `code_stack` from `spare_code_stacks` and gives a run's stack back
>   when it succeeds. A failing run keeps it for the error's stack, as before.
> - `DropStep::Glue`/`Fields` take `OwnedBytes`. Drop step stacks come from `spare_drops`
>   (`run_drop_glue`, `exec_fn_def`'s `FnOnce` frame) and go back through `keep_drops` when
>   done (`drop_steps`, `finish_drops`), if they hold memory. `drop_glue_step` pushes its
>   parts onto `steps` and reverses that run in place: no `parts` `Vec`, the same order. The
>   box and enum reads borrow memory instead of copying it.
> - The public `interpret_mir` moves `read_statics` into each round's evaluator and takes it
>   back (the evaluator only reads it): no clone.
> - §12 for `eval.rs`'s error, edge and setup sites: the assert messages, `MirOutput`,
>   stdout/stderr, the formatted result, the constant's map, the layout cache's and
>   `storage_marked`'s `Arc`s, and `Evaluator::new`'s stack and heap.
> - The hot lint for codegen-mir's paths went from 312 to 169. Many of those left are the
>   lint's own false positives (`smallvec![` and `Vec::new()`, `v2/requests/speed.md`). The
>   rest are shim and call paths §9.1 doesn't list.

### 9.2 `src/mir/storage.rs`, `frame_storage_conflicts` (codegen calls it per function)

Today it allocates per statement: `before = storage.clone()`, `storage_at.push(..)`,
`after.clone()` and `before.clone()` of `DenseBitSet`s. A `DenseBitSet` is inline up to 128
bits and on the heap past that. `through_terminator` makes a new set per block per round, and a
call's places go into a `Vec`. It becomes one `frame::task(Tag::Mir, ..)` per call, with
`framed::Bits` sets, and the result built once.

```rust
// before
pub struct StorageConflicts {
    always: DenseBitSet<usize>,
    conflicts: BitMatrix<usize, usize>,
}
// after
pub struct StorageConflicts {
    /// The ones that have storage all through the body, as words.
    always: Box<[u64]>,
    /// Row `a` is `words` words from `a * words`.
    conflicts: Box<[u64]>,
    words: usize,
}

impl StorageConflicts {
    pub fn conflict(&self, a: usize, b: usize) -> bool {
        let bit = |words: &[u64], at: usize| words[at / 64] & (1 << (at % 64)) != 0;
        a == b
            || bit(&self.always, a)
            || bit(&self.always, b)
            || bit(&self.conflicts[a * self.words..(a + 1) * self.words], b)
    }
}
```

- The fields are private and `conflict` is the only reader (`hir-ty-codegen/mod.rs:13335`), so
  no caller changes. `#[derive(Debug, Clone)]` stays.
- In the body:
  - `index` (`vec!`) becomes a frame slice.
  - `marked`, `borrowed`, `always` and `tracked` become `Bits`: `new_filled` is `insert_all`,
    and `subtract`/`intersect` as in §2.
  - `entry: Vec<Option<Bits>>`, `holding: Vec<Bits>` and `worklist` become frame `Vec`s.
  - Line 126, `entry[..].clone()`, becomes `live.copy_from(..)` into one reused set.
  - `through_terminator` writes into a reused `&mut Bits`.
  - Line 206, `places` (a `Vec` of `StoredPlace` clones, minus the last), becomes two visits:
    count the places, then read all but the last:
    `let mut n = 0; visit(kind, &mut |_| n += 1); let mut i = 0; visit(kind, &mut |it| { i += 1; if i < n { read(&mut live, it) } });`.
    Same places, same order, no `Vec`.
  - Lines 264-291, `storage_at`, becomes one frame `Vec<Bits>` reused across blocks (`clear`,
    and refill with `copy_from` of reused sets; grown, never shrunk).
  - Lines 266, 324 and 326, the `clone`s, become `copy_from` into reused sets.
  - The conflicts matrix is a frame `Vec<u64>` of `count * words` words, and `all_pairs` ORs
    `at_once`'s words into each member's row:
    `for at in at_once.iter() { for (w, &b) in row_mut(at).iter_mut().zip(at_once.words()) { *w |= b } }`.
    That is what `union_row_with` does.
  - At the end, `StorageConflicts { always: Box::from(always.words()), conflicts: Box::from(&matrix[..]), words }`
    gives exactly 2 allocations, the result's (0 for `count == 0`).
- `use rustc_index::bit_set::{BitMatrix, DenseBitSet};` goes from this file.

Budget: `storage_conflicts_allocate_their_result_only` (§10).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 9.3 `src/mir/type_name.rs` (codegen calls `rustc_debug_function` per function)

`Spelled.text` starts as `String::new()` and grows by doubling, and every entry function returns
it with spare capacity. Nested `Spelled::new`s (`write_dyn` 511-529, `write_dyn_debug` 717-760,
line 564) each make another `String`. The text is written into a lent scratch `String`, and the
result is one exact copy:

```rust
// before
struct Spelled {
    text: String,
    spelling: Spelling,
}
impl Spelled {
    fn new(spelling: Spelling) -> Self {
        Self { text: String::new(), spelling }
    }
// after
struct Spelled {
    text: blaze_mem::scratch::Lent<String>,
    spelling: Spelling,
}
impl Spelled {
    fn new(spelling: Spelling) -> Self {
        let mut text = blaze_mem::scratch::lend_string();
        text.clear();
        Self { text, spelling }
    }

    /// The name written, allocated once at its length.
    fn into_name(self) -> String {
        String::from(self.text.as_str())
    }
```

- The entry functions (lines 36-131, 7 of them) end with `Some(out.into_name())` instead of
  `Some(out.text)`.
- The nested parts (`parts.push(part.text)`, `autos.push(part.text)`, 517, 524, 727, 746-747)
  become `part.into_name()`. They are the parts `join` and `sort` order. Those `Vec<String>`s
  are on `dyn` types only (`// alloc: dyn types' auto-trait names, sorted as rustc sorts them; not on the per-function path`).
- `lend_string` keeps `DEPTH` buffers warm per thread. A nesting deeper than that allocates,
  which is correct and only slower.
- Every `self.text.push_str` and `write!(self.text, ..)` site is unchanged: `Lent<String>`
  derefs mutably to `String`.

Budget: `type_names_allocate_the_name_only` (§10).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 9.4 `src/mir/purity.rs`, `operands(rvalue) -> Vec<&Operand>` (codegen's `fncache.rs` calls it per statement)

```rust
// before
pub fn operands(rvalue: &Rvalue) -> Vec<&Operand> {
    match rvalue {
        .. => vec![it],
        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => vec![a, b],
        Rvalue::Aggregate(_, it) => it.iter().collect(),
        .. => Vec::new(),
    }
}
// after
pub fn operands(rvalue: &Rvalue) -> impl Iterator<Item = &Operand> {
    let (one, two, many): (Option<&Operand>, Option<&Operand>, &[Operand]) = match rvalue {
        .. => (Some(it), None, &[]),
        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => (Some(a), Some(b), &[]),
        Rvalue::Aggregate(_, it) => (None, None, it),
        .. => (None, None, &[]),
    };
    one.into_iter().chain(two).chain(many)
}
```

- The operands come in the same order.
- Callers: `purity.rs:194` (`for operand in operands(rvalue)`) and `purity.rs:332`
  (`operands(rvalue).into_iter().any(..)`), and `hir-ty-codegen/fncache.rs:1619` (`.into_iter().any`)
  and `:2105` (`for`). All compile unchanged: `into_iter()` on an iterator is itself. No change
  is needed in `hir-ty-codegen`.
- `const_purity`'s own temporaries (lines 77-200, the `(args.iter().collect(), ..)` at 123 and
  the walk's sets) become frame `Vec`s and sets in one `frame::task` per call.
- The `Impure` strings are its result (`// alloc: the reason, kept`).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store (2026-10-29), uncompiled: `operands` is the iterator as written (codegen-mir's edit, checked); `const_purity` has no temporaries left to move (the terminator's operands are a slice of `args`, no walk sets), and `Impure`'s strings carry `// alloc:`.

### 9.5 `src/mir/const_check.rs`

`const_check` (line 87) runs once per body for diagnostics. Its walk (lines 298-300: `seen`
`FxHashSet`, `stack`, `order`) goes into one `frame::task` as `Bits` and frame `Vec`s. The
`format!` messages are its result (`// alloc: the diagnostic's text, kept`).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: the check's block walk runs in one
> `frame::task(Tag::Mir, ..)`; `reachable` keeps `seen` as `framed::Bits` and `stack`/`order` as
> frame `Vec`s, and reads a `SwitchInt`'s targets directly (no spilled `SmallVec`). The messages
> already carry their `// alloc:` lines.

## 10. The budget tests

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 10.1 Hook: `src/mir/budget.rs` (new, `#[doc(hidden)] pub mod budget` in `mir.rs`)

```rust
//! What `tests/mir/alloc_budget.rs` measures: the pipelines' steps one at a time, outside
//! salsa, so a test can count what one step allocates.

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};

use crate::{
    InferBodyId,
    db::HirDatabase,
    next_solver::{
        DbInterner, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use crate::next_solver::StoredGenericArgs;

use super::{MirBody, MirLowerError, MirOptions, transform::Ctx};

/// A pass of the pipelines, by rustc's name for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    CleanupPostBorrowck,
    SimplifyCfg,
    RemoveStorageMarkers,
    RemoveUnneededDrops,
    UnreachablePropagation,
    InstSimplify,
    LowerSliceLenCalls,
    SimplifyConstCondition,
    SimplifyLocals,
    SingleUseConsts,
    DeadStoreElimination,
    CopyProp,
    CriticalCallEdges,
    RemoveNoopLandingPads,
    CheckAlignment,
    CheckNull,
    CheckEnums,
    AddAsserts,
    ElaborateDrops,
    /// Drop elaboration's analyses, without its rewrite: live blocks, move paths, maybe-init
    /// and maybe-uninit, dead unwinds.
    DropAnalysis,
}

/// The optimizations, each of which leaves a body it runs on a second time as it was.
pub const OPTIMIZATIONS: &[Pass] = &[
    Pass::CleanupPostBorrowck,
    Pass::SimplifyCfg,
    Pass::RemoveStorageMarkers,
    Pass::RemoveUnneededDrops,
    Pass::UnreachablePropagation,
    Pass::InstSimplify,
    Pass::LowerSliceLenCalls,
    Pass::SimplifyConstCondition,
    Pass::SimplifyLocals,
    Pass::SingleUseConsts,
    Pass::DeadStoreElimination,
    Pass::CopyProp,
    Pass::CriticalCallEdges,
    Pass::RemoveNoopLandingPads,
];

/// The context the passes of a body share, made once per body as the pipelines make it.
pub struct Passes<'f, 'db>(Ctx<'f, 'db>);

/// Runs `f` with the passes' context for `owner`'s body, in a frame of its own.
pub fn with_passes<'db, R>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    f: impl FnOnce(&Passes<'_, 'db>) -> R,
) -> R {
    frame::task(Tag::Mir, |frame: &Frame| f(&Passes(Ctx::new(db, owner, frame))))
}

impl<'db> Passes<'_, 'db> {
    /// `pass` on `body`, in a frame of its own, as the pipelines run it.
    pub fn run(&self, body: &mut MirBody<'db>, pass: Pass) -> Result<(), MirLowerError<'db>> {
        frame::task(Tag::Mir, |frame| super::optimize::run_one(&self.0, body, pass, frame))
    }
}

/// What `mir_body_query` builds for `def`, without salsa.
pub fn lower<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::lower::lower_body(db, def)
}

/// What lowering `def` builds that this crate does not own: the `Resolver` `MirLowerCtx::new`
/// makes, moved to the scopes of `tail` as a path there moves it, the `InferCtxt`, and the
/// function's signature with its late-bound regions freed, as `lower_body_to_mir` asks for it.
pub fn lowering_setup<'db>(db: &'db Db, def: InferBodyId<'db>, tail: hir_def::hir::ExprId) {
    // As `MirLowerCtx::new` (lower.rs:319-324) makes them.
    let mut resolver = def.resolver(db);
    let guard = resolver.update_to_inner_scope(db, def.expression_store_owner(db), tail);
    resolver.reset_to_guard(guard);
    let _ = new_infer_ctxt(db, def);
    // As `lower_body_to_mir` (lower.rs:5018-5024, after §5.7) asks for it.
    if let Some(func) = def.as_function() {
        let interner = DbInterner::new_with(db, def.krate(db));
        let _ = interner.liberate_late_bound_regions(
            func.into(),
            db.callable_item_signature(func.into()).instantiate_identity().skip_norm_wip(),
        );
    }
}

/// The `InferCtxt` that `Ctx::new`, `Filler::new` and `MirLowerCtx::new` build for `owner`.
pub fn new_infer_ctxt<'db>(db: &'db Db, owner: InferBodyId<'db>) -> InferCtxt<'db> {
    DbInterner::new_with(db, owner.krate(db)).infer_ctxt().build(TypingMode::non_body_analysis())
}

/// `run_passes`, `optimize` and `monomorphized`, as their queries call them.
pub fn run_passes<'db>(db: &'db Db, built: &MirBody<'db>) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::transform::run_passes(db, built)
}
pub fn optimize<'db>(db: &'db Db, body: &MirBody<'db>, options: MirOptions) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::optimize::optimize(db, body, options)
}
pub fn monomorphize<'db>(db: &'db Db, body: &MirBody<'db>, subst: &StoredGenericArgs, env: &StoredParamEnvAndCrate) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::monomorphization::monomorphized(db, body, subst, env)
}
```

Notes:
- `lower::lower_body(db, def)` is `mir_body_query`'s body (lines 4920-4956, after §5.6) moved
  into a `pub(crate) fn`. The query calls it.
- `transform::run_passes` becomes `pub(super)`.
- `lowering_setup`'s scope walk is how many `Resolver` scope-stack reallocations a body whose
  deepest path is at `tail` costs. A `Vec` grows by doubling from the same start, so the count
  depends only on the deepest stack reached, and `tail` is that point in the fixtures below.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).
> applied by codegen-mir-store (2026-10-29), uncompiled: `src/mir/budget.rs` and `#[doc(hidden)] pub mod budget;` in `mir.rs`, with `db: &'db Db`; without `lower` and `lowering_setup`, which come with §5's lowering test (step 6), as nothing names `lower::lower_body` yet. `Passes::run` calls `optimize::run_one` (codegen-mir's, §6.1).

> codegen-mir (2026-10-29): `budget.rs` is as written and takes `db: &Db`. `with_passes` and
> `Passes::run` go through `optimize::run_one` (§6.1). `lower`/`lowering_setup` are left
> out until the lowering budget test that calls them lands (§5, after D1/D2).

> applied by codegen-mir-store (2026-10-29), uncompiled: the rest of it, `budget::lower` (calls `lower.rs`'s `pub(crate) fn lower_body`, which `compute_mir_body` calls without its span) and `budget::lowering_setup`, as written.

### 10.2 `tests/mir/main.rs`

```rust
// after the `mod unwind;` line
mod alloc_budget;

/// Every allocation of the test binary, counted per thread, for `alloc_budget`'s budgets.
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

### 10.3 `tests/mir/alloc_budget.rs` (new)

```rust
//! Allocation budgets of MIR building, the passes and the interpreter, counted by the
//! allocator `main.rs` installs. A pass run again on a body it has run on allocates nothing;
//! building a body allocates the blocks the body keeps and the setup the front end owns, no
//! more; a warm interpreter call allocates nothing. What a pass keeps in the task's frame
//! is not an allocation: the frame is reserved once per thread and reused.

use blaze_alloc_count::measure;
use hir_def::hir::Expr;
use hir_ty::{
    db::HirDatabase,
    mir::{MirBody, MirOptLevel, MirOptions, return_slot},
    next_solver::GenericArgs,
};
use hir_ty_mir::mir::{
    FrameStorage,
    budget::{self, OPTIMIZATIONS, Pass},
    type_name::rustc_type_name,
};
use test_fixture::WithFixture;

use crate::{constant, db::TestDB, function};

/// Moves, a conditional move with a drop flag, a `match` on an enum, and a loop.
const WORK: &str = r#"
//- minicore: sized, drop, copy, option, add
struct Noisy(u32);
impl Drop for Noisy {
    fn drop(&mut self) {}
}
fn make(n: u32) -> Option<Noisy> {
    if n > 3 { Some(Noisy(n)) } else { None }
}
fn consume(_: Option<Noisy>) {}
pub fn work(a: u32, b: u32, flag: bool) -> u32 {
    let mut total = 0;
    let kept = make(a);
    let mut i = 0;
    while i < b {
        let moved = make(i);
        if flag {
            consume(moved);
        }
        total = total + i;
        i = i + 1;
    }
    match kept {
        Some(Noisy(n)) => total + n,
        None => total,
    }
}
/// Nothing that panics, drops or calls: the runtime passes leave its shape as it is.
pub fn plain(a: u32, b: u32) -> u32 {
    if a < b { a } else { b }
}
"#;

fn body_of(db: &TestDB, name: &str) -> hir_ty::InferBodyId<'_> {
    hir_ty::InferBodyId::DefWithBodyId(function(db, name).into())
}

/// How many heap blocks `body` owns: what a copy of it allocates.
fn owned(body: &MirBody<'_>) -> u64 {
    let (copy, used) = measure(|| body.clone());
    drop(copy);
    used.allocations
}

#[test]
fn warm_optimizations_allocate_nothing() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        budget::with_passes(&db, def, |passes| {
            for &pass in OPTIMIZATIONS {
                // The first run warms the frame, the interners, the memos and the context's
                // caches, and takes the body to where this pass leaves it.
                let mut once = runtime.clone();
                passes.run(&mut once, pass).expect("the pass");
                let mut again = once.clone();
                let (ran, used) = measure(|| passes.run(&mut again, pass));
                ran.expect("the pass, again");
                assert_eq!(used.allocations, 0, "{pass:?} run again on its own output");
                assert_eq!(again, once, "{pass:?} leaves its output as it is");
            }
        });
    });
}

#[test]
fn warm_ub_checks_allocate_nothing_without_sites() {
    // `work` reads no raw pointer and transmutes into no enum: the checks scan and add nothing.
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        budget::with_passes(&db, def, |passes| {
            for pass in [Pass::CheckAlignment, Pass::CheckNull, Pass::CheckEnums] {
                let mut once = runtime.clone();
                passes.run(&mut once, pass).expect("the check");
                let mut again = runtime.clone();
                let (ran, used) = measure(|| passes.run(&mut again, pass));
                ran.expect("the check, again");
                assert_eq!(used.allocations, 0, "{pass:?} on a body with nothing to check");
                assert_eq!(again, runtime, "{pass:?} adds nothing");
            }
        });
    });
}

#[test]
fn warm_runtime_analyses_allocate_nothing() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let built = db.mir_body(def).expect("the built body").clone();
        budget::with_passes(&db, def, |passes| {
            let mut warm = built.clone();
            passes.run(&mut warm, Pass::DropAnalysis).expect("the analyses");
            let mut measured = built.clone();
            let (ran, used) = measure(|| passes.run(&mut measured, Pass::DropAnalysis));
            ran.expect("the analyses, again");
            assert_eq!(used.allocations, 0, "move paths and the init analyses, warm");
            assert_eq!(measured, built, "the analyses change nothing");
        });
    });
}

#[test]
fn run_passes_allocates_the_kept_body_only() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "plain");
        let built = db.mir_body(def).expect("the built body").clone();
        let warm = budget::run_passes(&db, &built).expect("the passes");
        let (infcx, setup) = measure(|| budget::new_infer_ctxt(&db, def));
        drop(infcx);
        let (ran, used) = measure(|| budget::run_passes(&db, &built));
        let ran = ran.expect("the passes, again");
        assert_eq!(ran, warm);
        // The one copy the query keeps, its promoted constants at their length (none here),
        // and the context's `InferCtxt`; nothing the passes use for themselves.
        assert_eq!(used.allocations, owned(&ran) + setup.allocations);
    });
}

#[test]
fn optimize_allocates_the_kept_body_only() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let options = MirOptions::at(MirOptLevel::Release);
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        // At its fixed point, no pass removes anything, so the copy is all that is kept.
        let fixed = budget::optimize(&db, &budget::optimize(&db, &runtime, options).unwrap(), options).unwrap();
        let (infcx, setup) = measure(|| budget::new_infer_ctxt(&db, def));
        drop(infcx);
        let (ran, used) = measure(|| budget::optimize(&db, &fixed, options));
        let ran = ran.expect("the optimizations");
        assert_eq!(ran, fixed, "the optimizations at their fixed point");
        assert_eq!(used.allocations, owned(&fixed) + setup.allocations);
    });
}

#[test]
fn monomorphize_allocates_the_kept_body_only() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        let subst = GenericArgs::empty().store();
        let env = hir_ty::traits::ParamEnvAndCrate {
            param_env: db.trait_environment(def.generic_def(&db)),
            krate: def.krate(&db),
        }
        .store();
        let warm = budget::monomorphize(&db, &runtime, &subst, &env).expect("the fill");
        let (infcx, setup) = measure(|| budget::new_infer_ctxt(&db, def));
        drop(infcx);
        let (ran, used) = measure(|| budget::monomorphize(&db, &runtime, &subst, &env));
        assert_eq!(ran.expect("the fill, again"), warm);
        // `work` is not generic: every type fills to itself and every constant keeps its
        // handle, so the copy is all that is kept.
        assert_eq!(used.allocations, owned(&warm) + setup.allocations);
    });
}

/// A body of `n` statements at one scope depth, with the same types in each: what lowering
/// allocates for itself must not grow with `n`.
fn straight(n: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy, add\npub fn straight(a: u32) -> u32 {\n    let mut x = a;\n");
    for _ in 0..n {
        text.push_str("    x = x + 1;\n");
    }
    text.push_str("    x\n}\n");
    text
}

/// What lowering `straight(n)` allocates beyond the body it keeps and its setup.
fn lowering_extra(n: usize) -> (u64, u64) {
    let (db, _) = TestDB::with_single_file(&straight(n));
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "straight");
        let warm = budget::lower(&db, def).expect("the body");
        // The block's tail `x`: the path with the deepest scope chain of the body.
        let body = hir_def::expr_store::Body::of(&db, function(&db, "straight").into());
        let Expr::Block { tail: Some(tail), .. } = &body[body.root_expr()] else {
            panic!("`straight` is a block with a tail")
        };
        let tail = *tail;
        let ((), setup) = measure(|| budget::lowering_setup(&db, def, tail));
        let (ran, used) = measure(|| budget::lower(&db, def));
        let ran = ran.expect("the body, again");
        assert_eq!(ran, warm);
        (used.allocations - owned(&ran), setup.allocations)
    })
}

#[test]
fn lowering_allocates_the_kept_body_and_its_setup_only() {
    let (small, setup) = lowering_extra(8);
    let (large, _) = lowering_extra(64);
    assert_eq!(small, large, "lowering's own allocations grow with the body");
    assert_eq!(small, setup, "lowering allocates beyond the body and its setup");
}

#[test]
fn warm_interpreter_calls_allocate_nothing() {
    const LOOPS: &str = r#"
//- minicore: sized, copy, add
const fn run(n: u32) -> u32 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        sum = sum + i;
        i = i + 1;
    }
    sum
}
const WARM: u32 = run(5);
const TEN: u32 = run(10);
const TWENTY: u32 = run(20);
"#;
    let (db, _) = TestDB::with_single_file(LOOPS);
    hir_ty::attach_db(&db, || {
        let eval = |name| {
            measure(|| {
                db.const_eval(constant(&db, name), GenericArgs::empty(), None)
                    .expect("the constant")
            })
            .1
            .allocations
        };
        eval("WARM");
        // The two constants' bodies and runs are the same but for the count: each round of
        // the loop and each call of its arithmetic allocates nothing.
        assert_eq!(eval("TEN"), eval("TWENTY"));
    });
}

#[test]
fn storage_conflicts_allocate_their_result_only() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let body = db.optimized_mir_body(def, MirOptions::at(MirOptLevel::Debug)).expect("the body");
        let locals: Vec<_> = body.locals.iter().map(|(id, _)| id).collect();
        let _ = body.frame_storage_conflicts(&locals, |_| false);
        let (conflicts, used) = measure(|| body.frame_storage_conflicts(&locals, |_| false));
        drop(conflicts);
        assert_eq!(used.allocations, 2, "the set of locals always live, and the matrix");
    });
}

#[test]
fn type_names_allocate_the_name_only() {
    let (db, _) = TestDB::with_single_file(WORK);
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "make");
        let body = db.mir_body(def).expect("the body");
        let ty = body.locals[return_slot()].ty.as_ref();
        let _ = rustc_type_name(&db, ty);
        let (name, used) = measure(|| rustc_type_name(&db, ty));
        let name = name.expect("`Option<Noisy>` has a rustc spelling");
        assert_eq!(name.len(), name.capacity(), "the name at its length");
        assert_eq!(used.allocations, 1, "`Option<Noisy>`'s name, once");
    });
}
```

What each number is, and why:

| Test | Budget | Why |
|---|---|---|
| `warm_optimizations_allocate_nothing` | 0 | Every temporary is in the frame (§6). A pass at its fixed point removes nothing, so it rebuilds no kept container (`simplify_cfg`, `simplify_locals`, `renumber_*` skip the rebuild when nothing went). |
| `warm_ub_checks_allocate_nothing_without_sites` | 0 | Same, for a body with nothing to check. |
| `warm_runtime_analyses_allocate_nothing` | 0 | §4.5's dataflow, all in the frame. |
| `run_passes_allocates_the_kept_body_only` | the clone count of the result + the `InferCtxt` | On `plain`, the passes neither add nor remove anything, so the one copy is all that is kept. |
| `optimize_allocates_the_kept_body_only` | the clone count of the input + the `InferCtxt` | At the fixed point, the copy is all that is kept. |
| `monomorphize_allocates_the_kept_body_only` | the clone count + the `InferCtxt` | `work` is not generic, so every type fills to itself; with the §8 fix every constant keeps its handle. |
| `lowering_allocates_the_kept_body_and_its_setup_only` | equal extra at 8 and 64 statements, and extra = setup | Lowering's own allocations are independent of body size (exactly 0 per statement), and the constant part is exactly the `Resolver` (with its scope stack at its deepest) and the `InferCtxt`. Holds once D1 and D2 (§11) land: until then each constant operand's `Box<[u8]>` and the `Copy` cache's growth are extra. |
| `warm_interpreter_calls_allocate_nothing` | equal for 10 and 20 rounds | 0 per round; needs §9.1. |
| `storage_conflicts_allocate_their_result_only` | 2 | The two owned boxes of `StorageConflicts`. |
| `type_names_allocate_the_name_only` | 1 | The returned `String`, at its exact length. |

The `run_passes` budget is exact for a body whose shape the passes keep. On a body where they
add `Assert` blocks and drop ladders, those are kept-body growth (amortized `Vec`s, shrunk once
by `shrink_to_fit`, §3.2). The analyses that decide them are held to 0 by
`warm_runtime_analyses_allocate_nothing`.

> applied by codegen-mir-store (2026-10-29), uncompiled: in `tests/mir/alloc_budget.rs` (beside
> codegen-mir's three memo-hit tests and its storage and type-name budgets): the two warm
> optimization tests, the warm drop analyses, `run_passes_`/`optimize_allocates_the_kept_body_only`
> and `warm_interpreter_calls_allocate_nothing` (which holds once §9.1 is in), as written;
> `monomorphize_allocates_the_kept_body_only` compares a body of 1 and of 12 constants through
> the public `monomorphized`. Not yet: `lowering_allocates_the_kept_body_and_its_setup_only`,
> with §5 (step 6, after D1/D2).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> applied by codegen-mir-store (2026-10-29), uncompiled: the lowering test too (`straight`, `lowering_extra`, `lowering_allocates_the_kept_body_and_its_setup_only`), with the tail read through the column view (`body.expr(body.root_expr())`, `tail: Option<ExprId>` by value).

## 11. Needed from outside this crate

- **D1, `hir-ty` (`src/mir.rs:118-130` and `next_solver/allocation.rs`): intern a constant from
  borrowed bytes.**
  - The problem: `Operand::from_concrete_const(data: Box<[u8]>, ..)` and `from_bytes` box the
    bytes before interning. On a hit (every constant after the first of its value, which is
    nearly all of them on a warm build), the box is freed again. Lowering makes one per literal,
    discriminant and flag write, `elaborate_drops` one per flag statement, and asserts one per
    check.
  - The ask: `Allocation::from_slice(ty, memory: &[u8], memory_map: MemoryMap) -> Allocation`,
    which looks the row up by hash and `==` on the borrowed slice (an `Equivalent` lookup in
    `table::Values`) and boxes the bytes only on a miss. Then
    `Operand::from_slice(&[u8], ty) -> Operand` on top of it.
  - This crate's sites then change from
    `Operand::from_bytes(Box::new([value as u8]), ty)` to `Operand::from_slice(&[value as u8], ty)`
    (`rg 'from_bytes\(|from_concrete_const\('` in `src/mir`), and the same for `Box::default()`
    (`const_zst`).
  - Without D1, the lowering budget test fails by the number of constants, which is how it
    should fail.
- **D2, `hir-ty` (`src/mir.rs:1727`): `MirBody::mark_moves`'s `copy` parameter.**
  - The problem: it takes `&mut FxHashMap<Ty<'db>, bool>`, which pins lowering's `Copy` cache
    (`MirLowerCtx::copy`, asked on every statement that may panic) to the global allocator. It
    grows log(types) times per body.
  - The ask: make it generic over the allocator,
    `copy: &mut hashbrown::HashMap<Ty<'db>, bool, FxBuildHasher, A>` with
    `A: core::alloc::Allocator`. It only uses `entry(..).or_insert_with`, which hashbrown's map
    has. Or a `&mut impl FnMut(Ty<'db>) -> bool`.
  - Then `CopyCache<'f, 'db> = FrameMap<'f, Ty<'db>, bool>` here.
  - Until then, `copy` stays an `FxHashMap`:
    `// alloc: mark_moves (hir-ty) takes this map by type; frame once D2 lands`.
- **D3, root `Cargo.toml`: hashbrown's `nightly` feature.** Already filed by mem-db (§1).
- Nothing is needed from `hir-ty-codegen`: `operands` stays source-compatible (§9.4), and
  `StorageConflicts`' API is unchanged (§9.2).

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

## 12. What stays on the global allocator, with its `// alloc:` justification

The hot lint (`tools/v2/hot_lint.py`) reports 901 findings in this crate today. Sections 3-9
remove the ones on per-body and per-call paths. The ones left are listed below, and each gets
its `// alloc:` comment at the site so the lint passes them. The largest groups are:
- `pretty.rs`, 114;
- the string sites in `eval.rs`, 50, all errors;
- the shims, `simd.rs` 41, `shim.rs` 33, `reflection.rs` 30, `threads.rs` 17, `arch.rs` 16;
- `location.rs`, 20.

Each of these is a setup or error path, or output that is kept. Write the comment at the site:

- Every `MirLowerError`/`MirEvalError` construction with a `String` or `format!`:
  `// alloc: the error, which ends the query`.
- `pretty.rs` (80 sites): `// alloc: the text printed is the result (IDE and debugging only)`.
- `location.rs` (`placed`, `concat_parts`, `line_and_column`'s collect, 26-305):
  `// alloc: the location's text or table is the result`. Called per `file!()`/`line!()`
  expansion and per panic location, not per statement.
- `purity::Impure::said` and `const_check`'s messages: the diagnostic's text, kept.
- The interpreter's `Evaluator::new` (stack, heap, the `unused_locals_store`): one evaluation's
  setup, reused across its calls.
- Shims (`eval/shim.rs`, `simd.rs`, `reflection.rs`, `arch.rs`, `threads.rs`, `macro_call.rs`,
  `unwind.rs`): what a program asks the interpreter to make (a `Vec` for its allocation, a
  thread). The two `IntervalOrOwned::Owned` sites in `shim.rs` take `OwnedBytes` (§9.1).
- `type_name.rs`'s `dyn` paths (§9.3).
- `monomorphization::opaque_of_impl` (§8).
- `set_*_capacity`, the query declarations, `passes_not_ported`: no allocation.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

## 13. Order to land in

Each step builds and keeps the output bytes the same:

1. §1, §2, §10.1 and §10.2 (the hook with the passes it can reach today), and the tests that
   already hold:
   - `storage_conflicts_allocate_their_result_only` with §9.2;
   - `type_names_allocate_the_name_only` with §9.3.
2. `Ctx<'f, 'db>`, the optimize pipeline and every optimization (§6), with
   `warm_optimizations_allocate_nothing`, `warm_ub_checks_allocate_nothing_without_sites` and
   `optimize_allocates_the_kept_body_only`.
3. The runtime passes (§3, §4), with `warm_runtime_analyses_allocate_nothing` and
   `run_passes_allocates_the_kept_body_only`.
4. Monomorphization (§8), with its test.
5. The interpreter (§9.1), with its test.
6. Lowering (§5), with its test, once D1 and D2 are in. §5.6 and §5.7 can land in step 1.
7. The inliner (§7), when it is registered.

> applied by codegen-mir (2026-10-29), step 1, uncompiled (the crate waits on speed-hir-store's
> Statement-columns change; this lane builds nothing):
> - §1: `blaze-mem` and `hashbrown` deps, `#![feature(allocator_api)]`, `mod framed;`.
>   `pub mod budget` waits for step 2: its `Passes` holds the `Ctx<'f, 'db>` of §3.1.
> - §2: `framed.rs` with `Bits` and the methods `storage.rs` uses. `FrameMap`/`FrameSet`,
>   `is_empty` and the `Bits` of dse/move_paths arrive with their users in steps 2 and 3,
>   so nothing is dead code meanwhile.
> - §9.2 `storage.rs`, as written, plus one thing the spec missed: `successors()` copies a
>   `SwitchInt`'s targets into a `SmallVec<[_; 2]>`, which spills for a `match` with a third
>   target (an enum's `otherwise`), so the walk reads `all_targets()` itself (`next_blocks`).
>   Without that the budget of 2 fails on `work`.
> - §9.3 `type_name.rs` (`Lent<String>`, `into_name`), §9.4 `operands` as an iterator
>   (`hir-ty-codegen/fncache.rs:1619`'s `.into_iter()` is now clippy's `useless_conversion`;
>   codegen's to drop), §5.6, §5.7.
> - §10.2, and §10.3's `storage_conflicts_allocate_their_result_only` and
>   `type_names_allocate_the_name_only` in `tests/mir/alloc_budget.rs`, with three memo-hit
>   tests (`mir_body`, `optimized_mir_body`, `const_eval`: 0 allocations).
> - Next: step 2 (§6 with `Ctx<'f>`), then 3, 4, 5. Step 6 waits on D1 and D2 (hir-ty).
> - §10.1's `budget.rs` is written against `&'db dyn HirDatabase`; the database is concrete
>   now (`base_db::Db`, common.md), so it lands taking `db: &'db Db`.

> forwarded to codegen-mir (the MIR crate's holder) (from requests, pass 16; told by tmux).

> mem-codegen (2026-10-29 20:53): every `&'db dyn HirDatabase` in this request's code is now
> `&'db Db` (`base_db::Db`, as the tree has it: `optimize.rs:36`), so no request brings back a
> `dyn` database; `budget.rs` gains `use base_db::Db;`. Told codegen-mir.
