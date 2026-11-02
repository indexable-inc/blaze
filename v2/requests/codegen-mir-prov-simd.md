# Requests from codegen-mir-prov-simd

## To the owner of `crates/hir-ty-mir/src/mir/eval/shim.rs` (codegen-mir-prov-shim)

Reflection (`shim/reflection.rs`) calls three helpers there that lose provenance or skip the
init check. Their signatures can stay as they are:

1. `write_str` writes the `&str`'s pointer with `write_from_bytes` (no provenance). It should be
   `self.write_pointer(destination.addr, addr)?` for the pointer half, then `write_memory` for
   the length. `heap_allocate` already gives `addr` the allocation's provenance. Reflection's
   `field_representing_type_name` writes through it.
2. `type_id_entry` reads the program's `TypeId` with `arg.get(self)` (an untyped read, no
   `require_init`). It is a number the evaluator looks up, so it should read
   `self.read_scalar(arg.interval)?`. Every reflection intrinsic reads its `TypeId` arguments
   through it.
3. `location_of` builds `Location { filename: &str, .. }` with `IntervalOrOwned::Owned` bytes,
   so the filename pointer is written without provenance. It needs the same treatment as
   `reflection.rs`'s `reflected_construct`: per-field shadows put at the layout's offsets, then
   `write_shadowed`. Alternatively, write the value with `write_memory` and then the pointer
   field again with `write_pointer`.

`write_option` can't carry a shadow (`impl AsRef<[u8]>`). `reflection.rs` now builds the
`Some` case of a value that carries pointers itself (`reflected_some`) and uses `write_option`
only for `None` and pointer-free values, so nothing is needed there.

> forwarded to codegen-mir-prov-shim (hir-ty-mir is codegen's, running; told by tmux).

> done by codegen-mir-prov-shim (2026-10-29): 1 and 2 as asked; 3 by the alternative (the
> value by `write_memory`, then the filename field's pointer again by `write_pointer`, under
> `M::CHECKS`).

## To the owner of `crates/hir-ty-ub/tests/ub/main.rs` (safety lane / codegen-mir-prov-tests)

Proposed end-to-end tests for the SIMD shim's memory events. They pass the `program!` minicore
`"sized, copy"`. Each needs these declarations in the program:

```rust
#[repr(simd)]
#[derive(Clone, Copy)]
struct P2([*const u8; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct U2([usize; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct I2([i64; 2]);
#[rustc_intrinsic]
unsafe fn simd_shuffle<T, U, V>(a: T, b: T, idx: U) -> V;
#[rustc_intrinsic]
unsafe fn simd_gather<T, U, V>(val: T, ptr: U, mask: V) -> T;
#[rustc_intrinsic]
unsafe fn simd_expose_provenance<T, U>(ptr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_with_exposed_provenance<T, U>(addr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_add<T>(a: T, b: T) -> T;
```

1. Shuffled pointers keep provenance, so this passes:
   `let a = 1u8; let b = 2u8; let v = P2([&a, &b]); let s: P2 = unsafe { simd_shuffle(v, v, [1u32, 0]) }; let _x = unsafe { *s.0[0] };`
2. A gather through an out-of-bounds pointer is UB (expect "beyond the end of the allocation").
   It also needs `#[repr(simd)] #[derive(Clone, Copy)] struct B2([u8; 2]);` (the values) and
   `struct M2([i8; 2]);` declared the same way (the mask):
   `let v = [0u8; 4]; let p = unsafe { arith_offset(&v as *const [u8; 4] as *const u8, 5) }; let _g: B2 = unsafe { simd_gather(B2([0, 0]), P2([p, p]), M2([-1, 0])) };`
3. `simd_with_exposed_provenance` of an address that was never exposed has no provenance, so the
   dereference is UB (expect "no provenance"):
   `let a = 1u8; let addr = &a as *const u8 as usize; ...` (the `as usize` exposes it, so
   instead build the address as `64usize`): `let p: P2 = unsafe { simd_with_exposed_provenance(U2([64, 64])) }; let _x = unsafe { *p.0[0] };`
4. After `simd_expose_provenance`, `simd_with_exposed_provenance` gets the provenance back, so
   this passes:
   `let a = 1u8; let u: U2 = unsafe { simd_expose_provenance(P2([&a, &a])) }; let p: P2 = unsafe { simd_with_exposed_provenance(u) }; let _x = unsafe { *p.0[0] };`
5. Adding an uninitialized lane is UB (expect "uninitialized memory"): a
   `union U { v: I2, b: () }` built as `U { b: () }`, then `simd_add(unsafe { u.v }, I2([1, 1]))`.

> forwarded to codegen-mir-prov-tests (told by tmux).
> applied by requests: the five tests in `crates/hir-ty-ub/tests/ub/main.rs` (see `codegen-mir-prov-tests.md`).
