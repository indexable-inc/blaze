---
status: plan
summary: "Contextual teardown (`DropWith<C>`) on `#[blaze::linear]` types resolved by unique invariant-branded scope bindings, comptime composite derivation, hardware-floor 16-byte unmanaged `FrameVec`/`UnmanagedVec` parameterized by `I: LenIdx = u32` (`u16`/`u32`/`usize`), generalization to GPU/slab/IO handles, and the `#[blaze::capability]` firewall for `#![blaze::sans_io]` data."
date: 2026-11-27
---
# Contextual teardown: `DropWith<C>` and branded unmanaged containers

## Why

Explicit-allocator containers (`Vec<T, A>`) remove the ambient global heap, but storing the
allocator handle inside every container bloats every `AVec<'a, T>` to 32 bytes (four words) and
repeats the same pointer in every nested collection of an AST, HIR, or CFG. Storing no allocator
inside the container drops the header to **16 bytes** (`ptr: NonNull<T>, len: u32, cap: u32` — two
AAPCS64 registers, four vectors per 64-byte cache line, `Option<Vec>` still 16 bytes via the
`NonNull` niche), or **12 bytes** with `u16` indices so `(tag: u32, items: SmallFrameVec<'id, T>)`
packs into a single 16-byte word pair.

Unmanaged containers (`zig`'s `ArrayListUnmanaged`) normally pay for that layout in ergonomics and
safety: every early return, `?`, and panic path needs manual `deinit(alloc)`, and passing the wrong
allocator is UB. Combining blaze's existing `#[blaze::linear]` check ([linear.md](linear.md)),
invariant lifetime brands (`Brand<'id>`), and `DropWith<C>` eliminates both costs at compile time
with zero bytes of runtime overhead.

## 1. `trait DropWith<C>` and unique-binding resolution

```rust
pub trait DropWith<C>: Sized {
    #[blaze::consumes]
    fn drop_with(self, cx: C);
}
```

When a `#[blaze::linear]` type implements `DropWith<C>`, every implicit drop point in built MIR —
scope exit, `return`, `?`, `break`, `continue`, overwrite (`v = ..`), and panic unwind landing
pads — resolves teardown against the live lexical environment before falling back to a linearity
error:

1. **Uniqueness search.** At a drop of a live `#[blaze::linear]` place of type `T: DropWith<C>`,
   the front end (`hir-ty-linear` + MIR drop elaboration) searches all live, initialized, unborrowed
   bindings in enclosing lexical scopes for a binding that can produce `C` (either by copy/reborrow
   `&*b` when `C = &X`, or by exclusive reborrow `&uniq *b` when `C = &uniq X`).
2. **Exact match succeeds.** If **exactly one** candidate binding matches `C`, the compiler
   elaborates the drop into `DropWith::drop_with(place, cx)`. Because invariant lifetime brands
   (`BrandedAlloc<'id, A>`, `GpuDevice<'dev>`, `Slab<'s>`) give each scope or resource a distinct,
   ununifiable type `C`, uniqueness holds automatically even when multiple allocators or devices
   are alive in the same function.
3. **Zero or ambiguous match is a compile error.** If 0 bindings match `C`, or 2+ bindings match
   `C`, or the sole candidate is moved or mutably borrowed at the drop point, compilation stops
   with a `#[blaze::linear]` error naming the value, the required context type `C`, and every
   conflicting candidate binding (or the active borrow blocking it). There is never a fallback or
   tie-breaker heuristic (rule `no-fallback`).
4. **Drop order and borrowck.** Elaborated `drop_with` calls run in standard reverse declaration
   order, strictly before the binding providing `C` is dropped or invalidated. Because `drop_with`
   borrows `C` in MIR before borrowck runs, the borrow checker rejects any program where a
   `DropWith<C>` value outlives its `C` binding.
5. **Unwinding and `?`.** Unlike `linear.md`'s rule 6 (which requires a zero-argument `Drop` impl
   for abandonment on `?` or panic), a `DropWith<C>` value is torn down cleanly on both `?` exits
   and unwind landing pads using the in-scope `C` binding — without storing `C` inside the value.
   A panic inside `drop_with` during unwinding aborts, matching `Drop`.
6. **Explicit consumption.** Moving the value into a `#[blaze::consumes]` method (`v.free(&alloc)`)
   or returning it to the caller moves the place out, so drop elaboration emits nothing.

```rust
with_brand(|brand| -> Result<usize, AllocFailure> {
    let alloc = BrandedAlloc::new(heap, brand);
    let mut xs = UnmanagedVec::new(); // 16 bytes, #[blaze::linear]
    xs.try_push(10, &alloc)?;
    xs.try_push(20, &alloc)?;         // on Err(?), compiler inserts xs.drop_with(&alloc)
    Ok(xs.len())
    // scope exit: compiler inserts xs.drop_with(&alloc) before `alloc` drops
})
```

## 2. Composite derivation via comptime reflection

Under `#![feature(blaze_comptime)]` ([comptime.md](comptime.md)), structs, enums, and tuples that
hold `DropWith<C>` fields need no hand-written boilerplate:

```rust
// Structural blanket via comptime reflection (planned in core::reflect):
impl<T: Struct, C: Copy> DropWith<C> for T
where
    for<F: Field<T>> F::Type: DropWithOrDrop<C>,
{
    #[blaze::consumes]
    fn drop_with(self, cx: C) {
        // Destructure fields; tear down each field in declaration order with DropGuard
        // panic safety so every remaining linear field still runs drop_with on unwind.
        for const f in fields_of::<T>() {
            DropWithOrDrop::teardown(self.[f], cx);
        }
    }
}
```

- **Shared contexts (`C: Copy`, e.g., `&BrandedAlloc<'id, A>`):** every `DropWith<C>` field receives
  a copy of `cx`; ordinary non-linear fields run their normal `Drop`.
- **Exclusive contexts (`C = &uniq S`, e.g., `&uniq Slab<'s>`):** reflection reborrows `&uniq *cx`
  sequentially for each field in declaration order; panic safety runs via MIR unwind edges.
- **Enums and std wrappers:** `for const v in variants_of::<T>()` dispatches on the active variant
  and tears down only its payload fields. Until full comptime derivation lands in the front end,
  `crates/blaze-own/src/brand.rs` ships explicit `DropWith<C>` impls for `Option<T>`, `Result<T, E>`
  (when `E` is non-linear), and panic-safe `(T, U)` (for `C: Copy`).

## 3. Explicit binding syntax (`with(cx)` / `defer`) vs branded-scope resolution

Two explicit surface alternatives exist alongside branded-scope resolution:

| mechanism | ergonomics | cross-helper safety | hidden storage | role |
|---|---|---|---|---|
| **Branded-scope `DropWith<C>`** | zero syntax at use site | compile-time proof via `'id` | 0 B | **primary** (default for all branded handles) |
| **Explicit creation `let v = T::new() with(cx);`** | names `cx` once at binding | local to one binding | 0 B (MIR-only) | opt-in disambiguator when two unbranded `C`s coexist |
| **`defer { v.free(cx); }`** | manual per binding | none unless paired with brand | 0 B | explicit fallback for non-`DropWith` cleanup |

Branded-scope resolution is the primary design because `Brand<'id>` is already needed to prevent
cross-allocator `push`/`reserve`/`free` UB across helper calls; once `'id` is in the type, the
binding of `&BrandedAlloc<'id, A>` is already unique in scope with zero extra syntax.

Where a caller intentionally holds two unbranded contexts of identical type `C` in one scope,
`let v = expr with(cx);` (planned sugar) binds `v`'s MIR drop elaboration directly to `cx` without
storing `cx` in `v` (still 16 bytes; borrowck still enforces `cx` outlives `v`). If neither a
unique branded binding nor an explicit `with(cx)` clause is present, the linear checker refuses the
drop outright.

## 4. Hardware-floor unmanaged containers (`I: LenIdx = u32`) and compressed 32-bit pointers

Implemented today in `crates/blaze-own/src/{brand.rs,frame_vec.rs,arena_ptr.rs}` (`#![no_std]`):

- **Invariant generativity (`brand.rs`):** `Brand<'id>` (`PhantomData<fn(&'id ()) -> &'id ()>`,
  0 bytes, invariant in `'id`) is created only inside `with_brand(|brand| ...)`. Two `with_brand`
  scopes can never unify `'id1` and `'id2`.
- **Single generic index parameter (`I: LenIdx = u32`):** a sealed trait `LenIdx` implemented for
  `u16`, `u32`, and `usize` keeps one implementation for all three widths (rule
  `one-concept-one-home`, zero duplication):

| type / alias | `I` | size (64-bit) | `Option<Self>` | max elements / span | use case |
|---|---|---|---|---|---|
| `Ref32<'id, T>` | — | **4 B** (align 4) | **4 B** (`NonZeroU32`) | 32 GiB (`off << 3`) | single ARM64 `add` deref into `ArenaBase` |
| `ArenaVec<'id, T>` | `u16` (default) | **8 B** (align 4) | 12 B | `65,535` (32 GiB slab) | fits in **1 register**, 4× smaller than `AVec` |
| `ArenaVec<'id, T, u32>` | `u32` | **12 B** (align 4) | 16 B | `4,294,967,295` | large compressed vectors inside `ArenaBase` |
| `SmallFrameVec<'id, T>` / `SmallUnmanagedVec<'id, T>` | `u16` | **12 B** (align 4) | **12 B** (`NonNull`) | `65,535` | packs `(tag: u32, SmallFrameVec)` into **16 B total** (AST/HIR/CFG nodes) |
| `FrameVec<'id, T>` / `UnmanagedVec<'id, T>` | `u32` (default) | **16 B** (align 8) | **16 B** (`NonNull`) | `4,294,967,295` | default everywhere; 2 ARM64 regs (`x0, x1`), 4/cache line |
| `LargeFrameVec<'id, T>` / `LargeUnmanagedVec<'id, T>` | `usize` | **24 B** (align 8) | **24 B** (`NonNull`) | `isize::MAX / size_of::<T>()` | rare > 4 GiB raw byte buffers |

`Small*` uses a 4-byte-aligned non-null pointer wrapper (`#[repr(C, packed(4))]` over `NonNull<T>`)
so its size is 12 bytes without tail padding, while `u32` and `usize` store plain `NonNull<T>` at
natural 8-byte alignment. Overflow past `I::MAX` always returns `Err(AllocFailure::CapacityOverflow)`
(or panics in infallible methods) — never silent truncation (rule `no-fallback`). Zero-sized types
never allocate and report capacity `I::MAX`.

Three container tiers share the `Brand<'id>` foundation:

- **`ArenaBase<'id, A>` + `Ref32<'id, T>` + `ArenaVec<'id, T, I = u16>`** (compressed-pointer slab):
  a 64-byte-aligned contiguous bump slab reserving slot 0 (first 8 bytes) so every allocation has a
  `NonZeroU32` 8-byte slot offset (`base + (off << 3)`, one ARM64 `add x0, base, w_off, uxtw #3`,
  32 GiB span). `Ref32` is 4 bytes (`Option<Ref32>` = 4 bytes) with shared `get(&self)` and
  exclusive `get_mut(&mut self)`. `ArenaVec` is 8 bytes (one 64-bit register) and implements
  `DropWith<&ArenaBase<'id, A>>` to run element destructors in place. `ArenaBase` has no
  same-brand `reset`, guaranteeing no `Ref32` or `ArenaVec` can dangle or alias across resets.
- **`FrameVec<'id, T, I = u32>`** (bump/frame arena): allocates through `&BrandedBump<'id, A>`
  (constructible only from `A: BumpAlloc` — `&Frame`, `&Inline<N>`, `Bump<'a>`, `ArenaLocal`).
  Growth allocates a fresh slice and copies elements without calling `dealloc` on the old bump
  slice. `Drop` runs `ptr::drop_in_place` on `0..len` (compiled away for non-`Drop` types) with
  **zero `dealloc` calls**, because the arena reclaims all backing memory in O(1) on reset. Not
  linear.
- **`#[blaze::linear] UnmanagedVec<'id, T, I = u32>`** (general heap): allocates, grows, shrinks,
  and frees through `&BrandedAlloc<'id, A>`. Implements `DropWith<&BrandedAlloc<'id, A>>` by
  delegating to `#[blaze::consumes] fn free(self, alloc: &BrandedAlloc<'id, A>)`. Has no `Drop`
  impl: dropping without `free` or `DropWith` resolution is a compile error under blaze's linear
  checker.

## 5. Generalization beyond `Alloc`

`Brand<'id>` + `DropWith<C>` applies to any resource whose handles should remain compact,
POD-sized values tied to a scoped owner:

```rust
#[blaze::linear]
pub struct GpuBuffer<'dev> { handle: u64, size: u32, _brand: Brand<'dev> } // 16 B
impl<'dev> DropWith<&GpuDevice<'dev>> for GpuBuffer<'dev> { .. }

#[blaze::linear]
pub struct SlabBox<T, 's> { slot: u32, _brand: Brand<'s>, _owns: PhantomData<T> } // 4 B
impl<'s, T> DropWith<&uniq Slab<'s>> for SlabBox<T, 's> { .. }

#[blaze::linear]
pub struct Fd<'io> { raw: i32, _brand: Brand<'io> } // 4 B
impl<'io> DropWith<&Io<'io>> for Fd<'io> { .. }
```

In every case:
- Handles carry **zero** back-pointers to the device, slab, or IO driver.
- Invariant brands statically prevent freeing a `GpuBuffer<'dev1>` on `GpuDevice<'dev2>`, returning
  a `SlabBox<T, 's1>` to `Slab<'s2>`, or closing an `Fd<'io1>` on `Io<'io2>`.
- Unique-binding resolution tears handles down cleanly across `?`, early returns, and unwinds.

## 6. `#[blaze::capability]` firewall for `#![blaze::sans_io]`

`bstd::own::Capability` and the reflection-derived `NoCapability` walk ([ownership.md](ownership.md))
already check whether a type transitively owns or borrows a capability. `#[blaze::capability]`
promotes that check into a compiler-enforced boundary for sans-IO data:

1. **Marked capabilities:** `Alloc`, `SharedAlloc`, `Bump`, `BrandedAlloc`, `BrandedBump`, `Fs`,
   `Dir`, `Net`, `Clock`, `Rng`, `Env`, `Io`, `GpuDevice`, and `Mutex` / `RankedLock` carry
   `#[blaze::capability]` (and implement `bstd::own::Capability`).
2. **Non-storable in `#![blaze::sans_io]`:** any `struct` or `enum` defined inside a
   `#![blaze::sans_io]` crate/module (or marked `#[blaze::sans_io]`) is checked by the front end's
   ownership walk to satisfy `NoCapability`: no field may transitively own or borrow a
   `#[blaze::capability]` type. Violations are compile errors naming the exact field path
   (e.g., `ParserState.scratch: BrandedAlloc<'id, Frame>`).
3. **Why this pairs with `FrameVec` / `UnmanagedVec`:** pure sans-IO state machines cannot smuggle
   an allocator, filesystem, clock, or lock inside their data structs. Instead, their structs store
   compact 12/16-byte `FrameVec<'id, T, I>` / `UnmanagedVec<'id, T, I>` fields (which contain only
   `NonNull<T>`, `len`, `cap`, and 0-byte `Brand<'id>` — **zero** capability fields) and accept
   `&BrandedBump<'id, A>` or `&BrandedAlloc<'id, A>` purely as method arguments during state
   transitions.

## Implementation status

- **Built (`crates/blaze-own`):** `Brand<'id>`, `with_brand`, `BrandedAlloc<'id, A>`,
  `BrandedBump<'id, A>`, `DropWith<C>` (with impls for `Option<T>`, `Result<T, E>`, `(T, U)`),
  sealed `LenIdx` (`u16`, `u32`, `usize`), `FrameVec<'id, T, I = u32>`,
  `#[blaze::linear] UnmanagedVec<'id, T, I = u32>`, `Small*` / `Large*` aliases, and the
  compressed-pointer tier (`ArenaBase<'id, A>`, 4-byte `Ref32<'id, T>`, 8-byte
  `ArenaVec<'id, T, I = u16>`).
- **Planned (front end):** `hir-ty-linear` + MIR drop-elaboration unique-binding resolution for
  `DropWith<C>`, comptime reflection `DropWith` derivation over `fields_of` / `variants_of`,
  optional `let v = .. with(cx);` disambiguator, and the `#[blaze::capability]` check on
  `#![blaze::sans_io]` ADTs.
