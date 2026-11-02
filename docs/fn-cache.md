---
status: record
summary: #[blaze::cache]: results of pure functions kept across runs, keyed by the code they reach. Its library crates were deleted on 2026-10-30.
date: 2026-10-30
---
# Cached functions

Record: `crates/blaze-cache` and `crates/blaze-cache-macros`, the library and attribute below,
were deleted on 2026-10-30 (lane dedup, Andrew): nothing in the tree depended on them, and the
query database and `blaze-cas` keep blaze's own results across runs. What follows is the design
as it stood.

`#[blaze::cache]` on a pure function keeps what it returns, in memory for the run and on
disk for the runs after it. A run that calls it again with the same arguments gets the
result without running it, for as long as the function's code, and the code of everything
it calls, is what it was. An edit to a comment, the formatting, or a function it does not
reach keeps the results; an edit to anything it reaches drops them.
([#17](https://github.com/indexable-inc/blaze/issues/17))

```toml
[dependencies]
blaze-cache = { git = "https://github.com/indexable-inc/blaze" }
```

```rust
#[blaze::cache]
fn simulate(seed: u64, steps: u32) -> Vec<f64> {
    // ...
}
```

Under plain `cargo build` the attribute does nothing: every call runs, as it is written. Only
a program blaze built caches.

## What a result is kept by

The key of a call is three things:

- **The arguments**, written with serde in a form of their own (`crates/blaze-cache/src/key.rs`):
  each value says what it is and each sequence where it ends, and the entries of a std `HashMap`
  or `HashSet` of `RandomState` are sorted, so the same map is the same key whatever order its
  random seed gave its entries. Any other map keeps its order (an `IndexMap`'s is what it is).
- **The code.** The hash of the machine code blaze compiled for the function's body and of
  everything that code can reach: every function it calls, directly or through others, as the
  instance it is (`largest::<u32>` and `largest::<i64>` are two), every vtable it builds and the
  methods in it, and the value of every static it reads. It is the code without what says where
  it was written (the lines of each instruction, where variables are, where a panic happened),
  so moving lines or reformatting keeps it. Another cached function called from it counts by
  its body's code, so recursion is no problem.
- **The build**: the target, the toolchain, and the overflow checks the code was built with.
  The toolchain is its commit and the names of its library and compiler, and a hash of what the
  metadata of its `core`, `alloc` and `std` says of each crate (with the crate's hash, its SVH),
  so a library rebuilt under the same toolchain (`-Zbuild-std`, a patched sysroot) is another.

The hash is a salsa query, `cache_code`, of the database blaze compiles from
(`crates/hir-ty-codegen/fncache.rs`), so a daemon works it out once per edit, and two daemons
work out the same one.

Calls through a `dyn` reach only the vtables that code builds, and those are hashed with the
rest. A `dyn` can't be an argument.

## What can't be cached

A function is refused when what it returns could depend on more than its arguments. blaze
says why when the program first calls it, with where the reason is written:

```
blaze: `app::counted` is not cached: it reads the static `app::CALLS`, which can change, at src/main.rs:37:5
blaze: `app::noisy` is not cached: it reaches `std::io::stdio::_print` → …, which calls the foreign function `write`, at src/main.rs:43:5
```

It is refused when anything it reaches:

- reads or writes a static that can change (`static mut`, an atomic, a `Mutex`), a thread-local
  or a foreign static;
- calls a foreign function other than the allocator, `memcpy` and its kin, libm and LLVM's
  intrinsics, which is how I/O, time, randomness, the environment and threads are seen: the
  standard library gets them from the system. Of LLVM's intrinsics, those that read a counter,
  randomness, an address or the state of the machine, or set it, are refused (`llvm.x86.rdtsc`,
  `rdrand`, the floating-point environment);
- in a crate of the workspace, dereferences a raw pointer, takes an address as a number or has
  assembly in it;
- turns an address into data, which another run has another of: formats a pointer (`{:p}`),
  hashes or orders pointers, or asks for an address (`addr`, `expose_provenance`). Comparing
  two pointers for equality is fine;
- makes a `RandomState` other than for a std `HashMap` or `HashSet`, or gives the entries of one
  of `RandomState` in their order (`iter`, `keys`, `values`, `drain`, `retain`, `for` over it,
  `{:?}` of it, …) or its seed (`hasher`). Making one, looking up, inserting and `len` are fine:
  the seed orders the entries and nothing else;
- when an argument holds an `Rc` or an `Arc`, reads a count (`strong_count`, `get_mut`,
  `make_mut`, `try_unwrap`, `Weak::upgrade`, …) or `ptr_eq`: the key is the value, not who else
  holds it;
- can't be compiled by blaze.

The functions those are told by are always compiled as calls, never built into their caller,
so the walk sees them.

A `const fn` is pure by being one, for the most part: the const checker holds it to calling
only `const fn`s, taking no address as a number and running no assembly (`blaze check` gives
rustc's errors for those). What it can still do at run time is read a static that changes or
go through a raw pointer, and so can a `const fn` it calls. One query, `const_purity`
(`crates/hir-ty-mir/src/mir/purity.rs`), says whether a `const fn` does, why and where, and the
walk does not go past one that doesn't and has no generic arguments: what a generic one calls
by a trait can be a non-const `impl` at run time. `blaze check` warns of a `const fn` that does,
with where:

```
warning[const-impure]: const fn reads or writes mutable state: `CALLS` is `AtomicU64`, which is not `Freeze`
warning[const-impure]: const fn reaches `count`, which reads or writes mutable state: `COUNT` is a `static mut`
```

A `const fn`'s arguments are held to the rule below as any function's are: one that can
change behind a shared reference is refused, `const` or not.

The functions of `core` and `alloc` that aren't generic are called as rustc built them, so their
code is not hashed: the toolchain in the key stands for it. What they do is looked through
instead, in their MIR and that of the `core` and `alloc` they call, directly or through others,
for the same: `core::arch::x86_64::_rdtsc` reads the cycle counter and is refused, formatting a
number is not. A panic there is fine, as anywhere. Not looked through are what a generic body
calls by a trait of its parameters, what a `dyn` calls (the code reached builds that vtable)
and what dropping a value does.

Or when an argument can change behind a shared reference (it holds an `UnsafeCell`, as `Cell`,
`RefCell`, `Mutex` and atomics do; the counts of an `Rc` or an `Arc` are left to the rule above),
holds a raw pointer of a type of the workspace, a union of one, or a trait object.

A refused function runs every call, as under cargo.

This is conservative. A `HashMap` compared with `==` is refused (`eq` iterates one of them).
A map or set of another hasher keeps the order of its entries in the key, so the same entries
put in another order are another key: a miss, never a wrong result.

## What can be a value

Arguments are `Serialize`, the result `Serialize + DeserializeOwned`: numbers, strings,
`Vec`s, tuples, `Option`, `Result` and your own types with `#[derive(Serialize, Deserialize)]`.
The attribute takes plain `name: Type` parameters; not `self` (take `this: &Self`), patterns,
`impl Trait`, or an `async`, `const` or `unsafe` function.

## Where results are kept

In memory, up to 256 MiB for the run. On disk under `target/blaze/fn-cache` of the target
directory the program was built in, or `BLAZE_FN_CACHE_DIR`, up to 512 MiB
(`BLAZE_FN_CACHE_SIZE`), the entries used least recently going first. Each entry is written to
a file of its own and renamed into place, so a run that stops partway leaves a whole entry or
none, and each holds its key, so a damaged one is never taken for another.

| Variable | |
|---|---|
| `BLAZE_FN_CACHE=off` | caches nothing |
| `BLAZE_FN_CACHE=memory` | keeps results for the run alone |
| `BLAZE_FN_CACHE_DIR` | where results are kept on disk |
| `BLAZE_FN_CACHE_SIZE` | how much, `512M` by default |
| `BLAZE_FN_CACHE_LOG=1` | says for each call whether the result was found or computed |
| `BLAZE_FN_CACHE_TRACE=1`, set for the daemon | prints each function the code hash goes through |

A function's results on disk are kept for the code it has and the two codes before it: the
first call of a run with code the function did not have drops what any older code computed,
at once rather than when the size is reached.

v2 has no command that lists or clears what is kept: `blaze cache` was v1's. The library has
`clear_stale` (`crates/blaze-cache/src/store.rs`) with nothing calling it; deleting the
directory forgets everything.

## Live edits

v2 does not yet follow a cached function through a live edit. The function that asks for the
code holds the hash as a constant, so it has to be compiled again when any code its body reaches
changes. v1 did that with `cached_roots` in its code generator's patch module, deleted with v1's
whole-object build on 2026-10-30. v2's code identity of a body (`hir_ty_share::code`, read by
`crates/hir-ty-codegen/v2/hashes.rs`) takes each callee by its interface hash and does not hold
that constant.

## How it is built

- `crates/blaze-cache-macros`: the attribute. It keeps the body in a closure and has the
  function ask `blaze::__private::__blaze_cache_code(&closure)` for the code of it, then look
  the call up.
- `crates/blaze-cache` (the `blaze` library): the store and that function, whose body
  answers "unknown", so under rustc nothing is cached.
- `crates/hir-ty-codegen/fncache.rs`: blaze compiles a call of `__blaze_cache_code` into the
  hash (or why the function can't be cached) as a constant. Only crates with `blaze` as a
  dependency are looked through for such functions (`hir_ty::fn_cache::uses_fn_cache`), and
  nothing is hashed or compiled for the hash unless one is called, so a workspace without the
  attribute builds as it did before.

v1's `fncache.py` checked it end to end through v1's `cache` command; it was deleted on
2026-10-30 with that command, and v2 has no end-to-end check of it yet.
