---
status: current
summary: The general `blaze::` lints `blaze check` runs in every workspace (`blaze::general`): what each catches, why it costs, how to fix or allow it, and the `clippy.toml` keys that tune them.
---
# The general lints

`blaze check` runs clippy's lints and lints of its own, named `blaze::`. The lints of the group
`blaze::general` look only at the standard library and widely used crates (`Arc`, `HashMap`,
`BTreeMap`, `Vec`, `String`, atomics, `Result`, `Drop`, std's IO, arc-swap, crossbeam,
hashbrown, tokio, the common fixed-seed hashers, `core::fmt`, iterators, slices' sorts), so they are useful in any workspace. They are on at `warn` by default, everywhere.

The other `blaze::` lints (the group `blaze::internal`) check conventions of blaze's own code
base; they are allowed unless a workspace turns them on.

The group `blaze::pedantic_perf` (`deep_drop_at_exit`, `packable`, `pod_serialized_by_field`,
`soa_candidate`, `vecreport`) is speed advice that trades something else away or can't see whether the code is hot; it is
allowed unless a workspace or crate turns it on (`pedantic_perf = "warn"`; `vecreport`, which
the release build reports, only by its flags). `checkfree`,
`nopanic`, `noalloc`, `nonblocking` and `undeclared_effect` are in `blaze::general` but `deny` by
default, and fire only where the code asks: on functions marked with their attribute
(`#[blaze::checkfree]`, `#[blaze::nopanic]`, ...), and for `undeclared_effect` where a
`#[blaze::may(..)]` or `#![blaze::effects]` reaches the function.

Several lints come from ideas of the Goose language (no global heap, data stacks, bump
growth, inline nested data, narrow relative links, variable-size enums, in-place construction
and `checkfree` functions): `bounded_heap_field`, `box_large_new`, `boxed_recursive_node`,
`checkfree`, `deep_drop_at_exit`, `dyn_fn_param`, `enum_vec_padded`, `pod_serialized_by_field`,
`push_without_reserve`, `scratch_in_loop` and `wide_index`. Each section says which idea.

## Turning them on and off

Like clippy's lints, with an attribute on an item, a module or the crate:

```rust
#![allow(blaze::general)]          // the whole group, for this crate
#[allow(blaze::small_map)]         // one lint, for this item
#[expect(blaze::hash_twice, reason = "the lookup is cheap here")]
fn f() {}
```

or for every crate of a workspace, in the root `Cargo.toml` (cargo hands `[lints]` to rustc,
which rejects an unknown tool, so blaze reads its own table):

```toml
[workspace.metadata.blaze.lints]
general = "allow"
hash_twice = "deny"
```

A group applies before the single lints the table names, so `hash_twice = "deny"` wins over
`general = "allow"`. A rustc-style flag (`--allow=blaze::general`) does the same.

## Thresholds

The lints with a threshold read it from `clippy.toml` (or `.clippy.toml`), as clippy's lints do:

| key | lint | default |
| --- | --- | --- |
| `large-error-threshold` | `cold_error` | 128 (bytes; clippy's own key for `result_large_err`) |
| `outline-generic-body-threshold` | `outline_generic_body` | 30 (expressions) |
| `cache-line-size` | `false_sharing` | 128 (bytes) |
| `intern-candidate-uses` | `intern_candidate` | 8 (comparisons and clones) |
| `small-map-max-entries` | `small_map` | 8 (entries) |
| `unbuffered-io-writes` | `unbuffered_io` | 2 (writes to one stream outside a loop) |
| `unbuffered-io-small-read` | `unbuffered_io` | 64 (bytes; a read into a larger array is not counted) |
| `bounded-heap-field-string-bytes` | `bounded_heap_field` | 16 (bytes) |
| `bounded-heap-field-vec-len` | `bounded_heap_field` | 8 (elements) |
| `large-box-threshold` | `box_large_new` | 4096 (bytes) |

```toml
# clippy.toml
cache-line-size = 64
small-map-max-entries = 4
```

## A profile

Three lints read a profile of the code running: `profile_hot`, `stale_hot_marker` and
`hot_marked_cold`. A workspace names an LLVM indexed profile (a `.profdata`, as `llvm-profdata
merge` writes it from the `.profraw`s of a `-C profile-generate` run, or one of blaze's own PGO
profiles) in `clippy.toml`, the path from the file's directory, or in
`[workspace.metadata.blaze.lints]`, the path from the workspace's root; `clippy.toml` wins key by
key:

```toml
# clippy.toml
profile = "target/pgo/merged.profdata"
profile-hot-percent = 1       # a function at or above 1% of the profile's counts is hot
profile-cold-percent = 0.0001 # one below 0.0001%, or never run, is cold
```

With no profile named, the three say nothing. A profile named that can't be read, is not an
indexed profile (a raw `.profraw`, a text profile) or counts nothing is an error naming its
path; the lints never run without it. A lint result is cached under the profile's content, so a
new profile relints.

A function's weight is the sum of its records' counters (as LLVM's profile summary sums them),
over every instance of a generic function, and every closure and shim inside it. Names are
matched two ways: a profile of a program rustc built names functions by their symbols (v0 or
legacy, with the `file;` LLVM puts before an internal one), which blaze demangles and folds onto
the function in source (`krate::module::f`, `krate::module::Type::f`, `<Type as krate::Trait>::f`);
a profile of a program blaze built names each instance by a hash of its item and its arguments,
so only functions with no type or constant parameters are found in it. A function the profile
has no name for is neither hot nor cold. Two functions of a crate with the same key (impls of
one trait for a type with different arguments) are matched to neither. When the profile has hot
names in a crate's path that match no function of it, `blaze check` says so in a note, with the
count of the crate's functions it matched.

| key | lint | default |
| --- | --- | --- |
| `profile` | the profile lints | none (the lints are off) |
| `profile-hot-percent` | `profile_hot`, `hot_marked_cold` | 1 (percent of the profile's counts) |
| `profile-cold-percent` | `stale_hot_marker` | 0.0001 (percent of the profile's counts) |

## `arc_clone_only_borrowed`

**What it catches.** An `Arc` cloned (`x.clone()`, `Arc::clone(&x)`), or an arc-swap `ArcSwap`
loaded in full (`x.load_full()`), when the clone is only borrowed: used inline as the receiver
of a method of the value (`x.clone().len()`), or bound by `let y = ..` and every use of `y` is a
shared borrow (`y.len()`, `y.name`, `&y`, `*y`, a comparison). The `Arc` it came from must stay
put meanwhile: a place (not a temporary such as `m.lock().clone()`) reached through a shared
reference, a `static`, or a local nothing moves, assigns or borrows mutably.

**Why it matters.** Cloning an `Arc` is an atomic increment of its count and dropping the clone
an atomic decrement: two read-modify-writes of a cache line that every thread holding the `Arc`
shares. Under contention each one waits for the line to come back from another core.

**How to fix it.** Borrow the `Arc` you have: `&*x`, or call the method on `x` directly. For
`let y = x.clone();` with no type written, whose every use reads the value through `y`, the fix
is applied by `blaze check --fix`: `x.clone()` becomes `&*x`. For an `ArcSwap`, `load()` gives a
guard that borrows the current value without touching the count; keep `load_full()` for a value
kept past the scope. When a hot loop reads a shared `Arc` that another thread replaces often,
read it under epoch or hazard-pointer reclamation (arc-swap's guards, `crossbeam-epoch`,
`haphazard`): a read is a load and no count is touched.

**Not found.** A clone moved anywhere (a call's argument, a return, a struct field, a channel's
`send`, a spawned closure), any use inside a closure, and a source that may change while the
clone lives.

**Allowing it.** `#[expect(blaze::arc_clone_only_borrowed, reason = "..")]` on the function.

## `blocking_in_async`

**What it catches.** A call that blocks the thread, written directly inside an `async fn`, an
`async` block or an async closure: a free function of `std::fs`; `std::fs::File` and
`OpenOptions` methods; `std::thread::sleep`; reads, writes, connects and accepts of std's
`TcpStream`, `TcpListener`, `UdpSocket` and `Incoming`; reading `Stdin`; `recv` and `iter` of
std's `mpsc::Receiver` and crossbeam-channel's `Receiver`, and `send` of std's `SyncSender`;
`Command::output`, `Command::status` and `Child::wait`; `JoinHandle::join`. A call that is
awaited is not one of these.

**Why it matters.** An async executor runs many tasks on a few threads. While one task blocks
its thread, no other task scheduled there runs: timers fire late, sockets go unread, and with a
single-threaded runtime the whole program stops.

**How to fix it.** Use the async form (`tokio::fs`, `tokio::time::sleep`, `tokio::net`, an
async channel, `tokio::process`, awaiting the task's handle), or move the blocking work onto a
thread meant for it with `tokio::task::spawn_blocking` (or your runtime's equivalent).

**Not found.** Blocking inside a function the async code calls (only the async body itself is
read); a plain closure inside async code (it may run elsewhere); std's `Mutex` and `RwLock`
(short critical sections are fine, and holding a guard across an `.await` is clippy's
`await_holding_lock`); crossbeam's `Sender::send` (bounded and unbounded senders share a type).

**Allowing it.** `#[expect(blaze::blocking_in_async, reason = "..")]`.

## `borrow_workaround`

**What it catches.** A value looked up twice, first only to test that it is there and then
again in the branch that found it: `if m.contains_key(&k) { .. m.get(&k).unwrap() .. }` (or
`get_mut`, `&m[&k]`, `m[&k]`), the same with `if m.get(&k).is_some()`, and `if o.is_some() {
.. o.as_ref().unwrap() .. }` (or `as_mut`, `as_deref`, `as_deref_mut`) on an `Option`. The maps
are std's `HashMap` and `BTreeMap`, hashbrown's and `blaze_hash`'s, the same place and the same
key (a literal, `const`, local or place) both times. It fires only when the direct form is
proven to build and behave the same: blaze moves the second lookup before the `if` in the
body's MIR and runs its borrow checker on that, in the database's mode (`BLAZE_BORROWCK`) and
in stable Rust's (NLL); nothing may run between the test and the second lookup but borrows and
copies into temporaries, and the lookup's result is a reference (or an `Option` of one), which
has nothing to drop.

**Why it matters.** The first lookup hashes and probes (or walks the tree) for nothing: std
writes `contains_key(k)` as `get(k).is_some()`, so the direct form makes a subset of the same
`Hash` and `Eq` calls. The habit comes from borrow checkers that rejected the direct form; when
this lint fires, every checker accepts it. A borrow returned from the branch is
`nll_workaround` instead.

**How to fix it.** Look it up once: `if let Some(found) = m.get(&k) { .. found .. }`, where an
`unwrap()` of the second lookup becomes `found` and `&m[&k]` becomes `found`. `blaze check
--fix` applies it (machine-applicable unless a comment would be lost) and checks the crate
again, undoing the fix if it adds an error.

**Not found.** A `clone()` made to end a borrow, an index loop or raw pointer standing in for a
cursor, `mem::take` and put back, and the crate's own function called twice (it may have
effects): see [the plan](v2/plan/design/lint-borrow-workarounds.md). Concurrent maps (papaya,
dashmap), whose `get` hands back a guard; an `if` in a closure; code a macro wrote.

**Allowing it.** `#[expect(blaze::borrow_workaround, reason = "..")]` on the function.

## `bounds_check_in_loop`

**What it catches.** A `for` loop over a range (`for i in a..n`) whose body indexes a slice or
`Vec` by the loop variable (`xs[i]`, `xs[i + 1]`, `xs[i + k]`), when the range's end is not that
slice's `len()` and nothing just before the loop proves it in bounds: no reslice (`let xs =
&xs[..n];`) and no `assert!(n <= xs.len())` (a `debug_assert!` does not count, it is gone in
release). One finding per slice per loop. Arrays, and code a macro wrote, are not checked.

**Why it matters.** The compiler drops a bounds check only when it can prove every index in
range once, before the loop. Left in, the check runs every iteration, and its panic path keeps
the loop from being unrolled or vectorized.

**How to fix it.** Prove the bound once before the loop: reslice (`let xs = &xs[..n];`) or
`assert!(n <= xs.len());`. Or iterate (`xs.iter()`, `xs.iter().zip(ys)`, `windows`) instead of
indexing.

**Not found.** The proof is read from the text just before the loop, so a bound proved further
away (a check in the caller, a field set in a constructor) is not seen and the lint can fire on
code that is already fast; allow it there.

**Allowing it.** `#[expect(blaze::bounds_check_in_loop, reason = "..")]` on the function.

## `bounded_heap_field`

**What it catches.** A non-`pub` `String` or `Vec<T>` field of a struct with named fields,
whose every struct literal in the crate builds it from a literal or an empty value of at most
`bounded-heap-field-string-bytes` bytes (16) or `bounded-heap-field-vec-len` elements (8)
(`String::from("..")`, `"..".to_string()`, `vec![a, b]`, `Vec::new()`, `Default::default()`),
and that no body of the crate changes afterward (`frozen_collection`'s scan, which then leaves
the field to this lint; a `Vec<Vec<T>>` or `Vec<String>` is left to `jagged_vec`).

**Why it matters.** Each value allocates on the heap for a few bytes it could hold itself, and
every read follows a pointer to them. Goose keeps nested data like this inline in its records.

**How to fix it.** Hold it inline, sized for the bound: `[T; N]` when always full,
`arrayvec::ArrayVec<T, N>`, `smallvec::SmallVec<[T; N]>`, `arrayvec::ArrayString<N>` or a small
string type (`smol_str`, `compact_str`).

**Not found.** Tuple structs (built by a call, not a literal); a field built from a parameter or
any computed value, which is unbounded.

**Allowing it.** `#[expect(blaze::bounded_heap_field, reason = "..")]` on the struct.

## `box_large_new`

**What it catches.** `Box::new(value)` where the value's layout is over `large-box-threshold`
bytes (4 KiB) and blaze does not already build it in the box. Sizes clippy's
`large_stack_arrays` or `large_stack_frames` already report (when on) and a place moved in
while `blaze::large_move` is on are left to those lints.

**What it no longer catches.** blaze's `BoxInPlace` MIR pass builds a value of at least 4 KiB
in the box, in debug and release builds, when it has nothing to drop and is an array, tuple
or struct literal whose parts are literals and small values (`Box::new([0u8; 1 << 20])`,
`Box::new(Big([0; 1024]))`), or the result of a function call whose inputs have nothing to drop
(`Box::new(make())`): no stack copy and no frame that size, so nothing to report. See
[mir-passes.md](mir-passes.md#optimizations).

**Why it matters.** What is left is built on the stack and then copied into the heap: a value
already in a local, one built over branches (`if`, `match`, a block), one with something to
drop. That is a frame that size (in a debug build a 1 MiB value overflows a fiber's stack) and
a `memcpy` of all of it. Goose builds every value in its final place.

**How to fix it.** Build it in the heap: `vec![0; n].into_boxed_slice()` (and `.try_into()` for
a `Box<[T; N]>`), `Box::<T>::new_uninit()` and writing it, or `Box::default()`; or pass the
literal or the call to `Box::new` directly instead of through a local.

**Allowing it.** `#[expect(blaze::box_large_new, reason = "..")]` on the function.

## `boxed_recursive_node`

**What it catches.** A non-`pub` struct or enum of the crate, without `#[repr(..)]`, that links
to itself through the heap: a field of type `Box<Self>`, `Option<Box<Self>>`,
`Rc<RefCell<Self>>`, `Option<Rc<RefCell<Self>>>` or `Vec<Box<Self>>` (the last left to clippy's
`vec_box` when that is on).

**Why it matters.** One allocation per node to build, one free per node to drop (recursively,
so a long list can overflow the stack), and a pointer chase to wherever the allocator put the
next node on every step of a walk. Goose builds its trees in a bump region, with narrow
relative links.

**How to fix it.** Keep the nodes in an arena, `Vec<Node>`, and link them by a `u32` index
(`struct NodeId(u32)`): one allocation, nodes next to each other, freed at once.

**Allowing it.** `#[expect(blaze::boxed_recursive_node, reason = "..")]` on the type.

## `checkfree`

**What it catches.** A function marked `#[blaze::checkfree]` (a blaze extension, opt-in; it
changes diagnostics, never code) whose optimized code still makes a bounds check, an overflow
check, a division-by-zero check, or calls one of the toolchain's panicking functions
(`panic_bounds_check`, `panic_const_*`, `panic_fmt`, `unwrap_failed`, `expect_failed`, an
`unwrap` or `Index::index` the optimizer kept out of line). Each check is its own finding, at
its source line (an inlined callee's at the call), naming which check it is. It is `deny` by
default: the function asked for it.

`blaze check` reads the function's MIR after blaze's optimizations at the release level, with
inlining. The MIR optimizer does not prove ranges, so the bounds checks `bounds_check_in_loop`
counts as proven (a range loop's `v[i]` after `assert!(n <= v.len())`, or bounded by
`v.len()`) are left out as LLVM's to drop, and the release build checks the optimized code
itself. An overflow check a crate drops without `overflow-checks` (`+`, `-`, `*`, shifts,
negation) is left out, as release builds drop it. A panic the function writes itself (a
`core::panicking` call from its own `assert!`, `panic!` or `unreachable!`) is its own stated
check and is allowed; an `unwrap()` or a `v[i]` is not.

**Why it matters.** Goose's `checkfree` functions: a hot loop whose author proved every index
in range says so, and the compiler holds the code to it, instead of a check silently coming
back after an edit and keeping the loop from vectorizing.

**How to fix it.** Prove the index or the arithmetic before the loop (`assert!(n <= v.len())`,
a reslice `let v = &v[..n];`), use `get`, `checked_*` or `wrapping_*`, iterate instead of
indexing, or take the attribute off.

**Not found.** A call of a function kept out of line is followed only for the toolchain's
panicking functions above, never into a crate function's body.

## `cold_error`

**What it catches.** A function returning `Result<T, E>` whose error `E` is over 128 bytes by its
layout (`large-error-threshold`). Trait impls' methods and test code are not checked. This is
clippy's `result_large_err`.

**Why it matters.** A `Result` is as big as its largest variant, so every `Ok` the function
returns is as big as the error: each return copies it, and each caller reserves the stack for
it, though the error is rare.

**How to fix it.** Box the error (`Box<E>`) or the payload of its large variant, so the
`Result` holds a pointer.

**Allowing it.** `#[expect(blaze::cold_error, reason = "..")]` on the function.

## `collect_then_consume`

**What it catches.** An `Iterator::collect` into a `Vec`, `VecDeque`, `String`, hash or B-tree
collection whose one use the iterator could answer itself: a `for` loop or `.iter()` chain over
it, `.len()`, `.is_empty()`, `.first()`, `.contains(..)`, or being passed where the parameter is
`impl IntoIterator`. The collection is the `collect` itself or a plain `let v = ..` used once,
outside any loop or closure the `let` is not in. A set or map is found only where the question
is the same of its distinct keys (`is_empty`, a set's `contains`).

**Why it matters.** The collection is allocated, filled and dropped for one answer the
iterator gives without it.

**How to fix it.** Ask the iterator: `.count()`, `.next().is_none()`, `.next()`, `.any(..)`, or
iterate it directly. `blaze check --fix` rewrites `.collect::<Vec<_>>().len()` to `.count()` and
`.collect::<Vec<_>>().is_empty()` to `.next().is_none()` where the chain is plain.

**Not found.** A delayed use when a local the iterator reads is named after the `collect`
(collecting first may end a borrow or take items before they change).

**Allowing it.** `#[expect(blaze::collect_then_consume, reason = "..")]`.

## `deep_drop_at_exit`

In `blaze::pedantic_perf`, allowed by default.

**What it catches.** In the crate root's `fn main` (with no parameters), a local std collection
(`Vec`, `VecDeque`, `HashMap`, `HashSet`, `BTreeMap`, `BTreeSet`) whose elements own heap memory
themselves (`Vec<String>`, `HashMap<String, Vec<u8>>`), every type in it std's, only borrowed,
so it is dropped where `main` returns. A `main` that calls `std::process::exit` or
`std::mem::forget` is left alone.

**Why it matters.** Dropping it frees every element one at a time just before the process
exits, when the operating system is about to take all of its memory back at once. Goose frees
a whole region at once.

**How to fix it.** `std::mem::forget(names)` at the end of `main`, or flush what must be
written and call `std::process::exit(0)`.

**Why it is off.** Leaking at exit hides real leaks from leak checkers (Valgrind, LSan), and the
time saved matters only for large collections, which the lint can't see.

## `dos_hasher_on_input`

**What it catches.** In a library crate, an `insert`, `entry` or `get_or_insert*` on std's or
hashbrown's `HashMap` or `HashSet` whose hasher has a seed fixed for every map (FxHash,
foldhash's `FixedState`, FNV, `BuildHasherDefault` of any hasher but std's SipHash), in a
`pub fn`, when the key comes from a parameter of type `str`, `String`, `[u8]`, `Vec<u8>` or a
`Cow` of one of them: the parameter itself, something made from it (`name.to_owned()`), or a
local bound from those by a `let` or a `for` loop (`for word in text.split(' ')`). The hasher is
the map's type argument as resolved, so an alias such as `FxHashMap` is found.

**Why it matters.** Whoever chooses the keys can choose many with the same hash, since the hash
function and its seed are the same in every process. Each insert then walks one long chain, and
a few thousand keys take seconds (HashDoS). std's `RandomState` picks a new seed per map.

**How to fix it.** Use a randomly seeded hasher for maps keyed by outside input
(`std::collections::hash_map::RandomState`, foldhash's or hashbrown's default `RandomState`),
or keep the fixed hasher and bound how many keys a caller can add.

**Not found.** ahash's `RandomState` (whether its seed is fixed isn't in the type), keys from
input the function reads itself, keys from parameters of other types, and private functions.

**Workspaces that turn on `blaze::internal`.** This lint is allowed by default where
`blaze::internal` is on, since such a workspace keys maps by fixed-seed hashers on purpose (for
output that is the same on every run). Naming the lint itself (`dos_hasher_on_input = "warn"`)
turns it back on.

**Allowing it.** `#[expect(blaze::dos_hasher_on_input, reason = "..")]`.

## `dyn_fn_param`

**What it catches.** A non-`pub` function (not a trait's method or a trait impl's) that takes
a closure as a trait object, `&dyn Fn(..)`, `&mut dyn FnMut(..)` or `Box<dyn FnOnce(..)>`, and
calls it inside a `loop`, `while`, `for` or iterator closure of its own body. A function that
calls itself is left alone (a type parameter there is polymorphic recursion). It is
`dyn_in_hot_field` for parameters, and shares its reading of the type.

**Why it matters.** Each call goes through the vtable, an indirect call the compiler can't
inline, so the loop can't be unrolled or vectorized around it. Goose builds every call in place,
never through a table.

**How to fix it.** Take the closure as a type parameter, `f: impl Fn(u32) -> u32`. This costs
one copy of the function per closure it is called with.

**Allowing it.** `#[expect(blaze::dyn_fn_param, reason = "..")]` on the function.

## `dyn_in_hot_field`

**What it catches.** A field of a non-`pub` struct of the crate whose type is `Box<dyn Trait>`
or `&dyn Trait`, that a method of the struct calls (`(self.f)(..)`, `self.f.method(..)`) inside
a `loop`, `while`, `for` or iterator closure. A struct that implements a trait of the workspace,
or that the crate holds as the element of a collection (a slice, an array, a `Vec`, a map) in a
field or a signature, is left alone: there a trait object is likely the point, since one
collection's elements share one type.

**Why it matters.** Each call goes through the vtable: an indirect call the compiler can't
inline, so the loop around it can't be unrolled or vectorized, nor have work hoisted out.

**How to fix it.** Take the implementation as a type parameter (`struct S<F: Fn(u32) -> u32> {
f: F }`). The call becomes direct and inlinable. This trades code size: the struct's code is
compiled once per type it is used with, so binaries and builds grow. Keep the `dyn` where that
matters more than the loop.

**Not found.** A `Vec` of the struct that lives only inside a function body is not seen by the
collection check; a call made once per method call, from a loop in another function, is not
found.

**Allowing it.** `#[expect(blaze::dyn_in_hot_field, reason = "..")]` on the struct.

## `enum_vec_padded`

**What it catches.** A non-`pub` `Vec<E>` field the crate only pushes to, iterates and
measures (never indexes, overwrites or hands on), where the largest variant of the enum `E` is
at least 4 times its smallest (by the sum of its fields' layouts) and an element is at least
16 bytes. This is clippy's `large_enum_variant` for collections: an enum `large_enum_variant`
already finds is left to it when that lint is on.

**Why it matters.** Every element takes the largest variant's room, so most of the buffer can
be padding: a value alone pays for it once, a `Vec` of them once per element. Goose's
variable-size enums store each value in only the bytes its variant needs.

**How to fix it.** One `Vec` per variant when the order across variants doesn't matter, or a
packed byte stream each value writes its tag and its own fields into, which keeps the order.

**Not found.** Locals are not followed; a variant with a field of unknown layout skips the
enum.

**Allowing it.** `#[expect(blaze::enum_vec_padded, reason = "..")]` on the struct.

## `false_sharing`

**What it catches.** A struct shared between threads (held in a `static` or an `Arc`) whose
atomics, or a lock and an atomic, are written by different methods and can sit in one cache
line by the struct's layout. A field whose own alignment is a line or more (a
`#[repr(align(128))]` wrapper, `crossbeam_utils::CachePadded`) is padded and does not count.

**Why it matters.** A core writing one field takes the whole line from the core that wrote the
other, though they share no data: the two threads serialize on the line.

**How to fix it.** Give each field written apart its own line: wrap it in
`crossbeam_utils::CachePadded<..>` or a `#[repr(align(128))]` struct. 128 bytes is the pair of
64-byte lines arm64 and x86-64 prefetch together; set `cache-line-size` for another target.

**Allowing it.** `#[expect(blaze::false_sharing, reason = "read-mostly")]` on the struct, for a
field that is read far more than written, or written by one thread.

## `fmt_allocates`

**What it catches.** Inside `fmt` of an impl of `Display`, `Debug` or another `core::fmt`
trait, a `String` built (`to_string()`, `format!`, `String::from`, `collect`, `join`, ...) whose
value only reaches what the `Formatter` writes (`write_str`, `write!`, `pad`, `write_char`),
inline or through plain `let`s.

**Why it matters.** Formatting already writes into the caller's buffer. The `String` is a heap
allocation and a copy on every `fmt` call, and a `to_string()` of a `Display` value formats it
twice.

**How to fix it.** Write the parts into `f`: `write!(f, "{}-{}", a, b)`, `f.write_str(..)`, or
the value's own `x.fmt(f)`.

**Not found.** A `String` that also goes elsewhere (kept, measured, compared), or reaches the
formatter through a `match`, an `if`, a loop or a closure, is left alone.

**Allowing it.** `#[expect(blaze::fmt_allocates, reason = "..")]` on the `fmt`.

## `frozen_collection`

**What it catches.** A non-`pub` field of type `Vec<T>` or `String` that no body of the crate
changes after the struct is built: no `&mut` borrow, no mutating method (`push`, `iter_mut`),
no assignment into it, no `ref mut` binding of it.

**Why it matters.** A `Box<[T]>` (`Box<str>`) holds the same items in two words instead of
three (no capacity), and says in its type that they are frozen.

**How to fix it.** Make the field `Box<[T]>` or `Box<str>` and build it with
`into_boxed_slice()` / `into_boxed_str()` where the struct is made.

**Allowing it.** `#[expect(blaze::frozen_collection, reason = "..")]` on the struct.

## `front_vec_ops`

**What it catches.** `Vec::remove(0)`, `Vec::insert(0, x)` or `Vec::drain(..1)` inside a loop
(`loop`, `while`, `for`, an iterator closure). When the loop also pushes to the same `Vec`, the
message says it is used as a queue.

**Why it matters.** Each one shifts every other element by one place, so the loop is quadratic
in the length.

**How to fix it.** Use `std::collections::VecDeque` (`pop_front`, `push_front`, `push_back`),
each constant time. Or reverse the `Vec` once and work at its end with `pop` and `push`.

**Allowing it.** `#[expect(blaze::front_vec_ops, reason = "..")]`, for a `Vec` known to stay
a few elements long.

## `hash_twice`

**What it catches.** A lookup of a key in a hash map or set (`get`, `contains_key`, `contains`,
...) followed by a write of the same key to the same map (`insert`, `remove`, `entry`), with
nothing between them that may change either.

**Why it matters.** Each call hashes the key and probes the table; the second does again what
the first did.

**How to fix it.** Look the key up once: the entry API (`m.entry(k).or_insert(v)`), the value
`insert` or `remove` returns, or on papaya `get_or_insert_with` / `compute`. `blaze check --fix`
rewrites `if !m.contains_key(&k) { m.insert(k, v); }` to `m.entry(k).or_insert(v);`.

**Allowing it.** `#[expect(blaze::hash_twice, reason = "..")]`.

## `hot_marked_cold`

**What it catches.** A `#[cold]` function that the configured profile ([A profile](#a-profile))
shows hot: at or above `profile-hot-percent` of its counts.

**Why it matters.** `#[cold]` tells the optimizer calls to the function are unlikely: it keeps
the function out of line, lays its callers' code out with the call on the slow side and
optimizes it for size. On a path the profile shows taken often, each of those costs.

**How to fix it.** Take `#[cold]` off, or move the cold part (the error's formatting) into a
`#[cold]` function of its own. `#[inline(never)]` alone is not taken as a cold marker: it also
keeps a frame small or a symbol visible to a profiler.

**Allowing it.** `#[expect(blaze::hot_marked_cold, reason = "..")]`, when the profile's workload
is not the one the function is cold in.

## `intern_candidate`

**What it catches.** A non-generic type of the crate that is `Clone`, `PartialEq` and `Hash`
(derived or written), recursive (a field holds the type itself behind a `Box`, `Vec`, `Rc`,
`Arc` or another collection) or deep (owned heap data nested two levels or more, such as
`Vec<String>`), that the crate both uses as the key of a hash map or set (or a `BTreeMap`) and
compares with `==` or clones in at least 8 places (`intern-candidate-uses`). Only the crate's
own code is counted; the type's own impls are not.

**Why it matters.** Every `==` walks both trees, every hash walks one, every clone copies one,
and a map keyed by the type does all three on each lookup.

**How to fix it.** Intern it (hash-consing): keep each distinct value once in an interner and
pass a small `Copy` id, so `==` and hashing compare one integer and a clone is a copy. Crates
in use for this: `internment`, `lasso` and `string-interner` (strings), `hashconsing`,
`id-arena` / `la-arena`, and salsa's `#[salsa::interned]`.

**Allowing it.** `#[expect(blaze::intern_candidate, reason = "..")]` on the type.

## `jagged_vec`

**What it catches.** A non-`pub` field of type `Vec<Vec<T>>`, `Vec<String>`, `Vec<Box<[T]>>` or
`Vec<Box<str>>` that no body of the crate changes after the struct is built (the same scan as
`frozen_collection`, which then leaves the field to this lint). Structs with `#[repr(..)]` are
not checked.

**Why it matters.** Each row is its own heap allocation with its own header, scattered in
memory: building the field allocates once per row, and reading row after row misses the cache
once per row.

**How to fix it.** Store every row's items in one flat `Vec<T>` (one `String` for text) and a
`Vec<u32>` of where each row starts, one past the end last; row `i` is
`&items[offsets[i] as usize..offsets[i + 1] as usize]`. Two allocations, next to each other.

**Not found.** A local of such a type is not followed; the field it ends up in is what is
found.

**Allowing it.** `#[expect(blaze::jagged_vec, reason = "..")]` on the struct.

## `missing_size_hint`

**What it catches.** An `impl Iterator` for a struct of the crate that either:

- has no `size_hint`, though `next` steps an integer field (`self.i += 1`) and the struct holds
  what it walks (a slice, a `Vec`, an array, a boxed slice, or a second integer field such as
  an end); or
- has a `size_hint` that returns an exact `(n, Some(n))` with no `ExactSizeIterator` impl.

**Why it matters.** The default `size_hint` is `(0, None)`: `collect` and `extend` then grow
their buffer by doubling (reallocating and copying) instead of allocating once, and `zip`,
`chain` and `rev` lose the length. Without `ExactSizeIterator`, callers have no `len()`.

**How to fix it.** Return the items left from `size_hint`, and add an empty `impl
ExactSizeIterator for ..` when the count is exact.

**Allowing it.** `#[expect(blaze::missing_size_hint, reason = "..")]` on the impl.

## `native_contracts`

**What it catches.** A function of a crate with `#![feature(blaze_verify)]` that states its
contract with nightly's `#[core::contracts::requires(..)]` or `#[core::contracts::ensures(..)]`.
The attributes are found by what their paths resolve to, so a rename (`use
core::contracts::requires as pre;`) or a glob import is found too. In a crate without the
feature it says nothing.

**Why it matters.** blaze_verify's native clauses say the contract in the signature, where
blaze reads and verifies it. A function takes one form (blaze refuses both on one function),
and the attributes are nightly's form, run only under `-Zcontract-checks`.

**How to fix it.** `blaze check --fix` rewrites the attributes as clauses:

```rust
#[requires(x < 10)]
#[ensures(|r| *r > x)]
fn next(x: u32) -> u32 { x + 1 }
// becomes
fn next(x: u32) -> (r: u32)
    requires
        x < 10,
    ensures
        ({ let r = &r; *r > x }),
{ x + 1 }
```

A requires with declarations, `#[requires(let lim = 10; x < lim)]`, becomes `({ let lim = 10;
x < lim })`. The return is named by the closure's parameter when no parameter of the function
has that name, else by a fresh name (`ret`, `result`, ...); a function with no `->` keeps its
signature and the `ensures` reads `&()`. The fix is made only where the clauses mean what the
attributes did: nightly builds the `ensures` closure at the start and a native `ensures` reads
at the end, so there is none when the closure reads a local of the requires' declarations, a
`mut` parameter or one that is not `Copy`; nor when a clause can `return` or use `?`, another
attribute macro rewrites the function, or the return type has an `impl Trait`. The finding
says which. The `use` that imported the attributes is left for `unused_imports`.

**Allowing it.** `#[expect(blaze::native_contracts, reason = "..")]` on the function.

## `nll_workaround`

**What it catches.** `borrow_workaround`'s shapes where only blaze's borrow checker (the
Polonius alpha, also nightly Rust's) accepts the direct form, and stable Rust's (NLL) rejects
it: NLL problem case #3, a borrow returned from the branch that found it
(rust-lang/rust#54663, #21906):

```rust
if m.contains_key(&k) {
    return m.get_mut(&k).unwrap();
}
m.insert(k, 0);
```

**Why it matters.** The second lookup is there only for stable Rust's checker. A crate built
only by blaze pays for it on every call.

**How to fix it.** `if let Some(found) = m.get_mut(&k) { return found; }`, as
`borrow_workaround` writes it. The crate then builds only with blaze (or nightly Rust).

**Who it is on for.** It is in `blaze::internal`, so allowed by default: blaze cannot tell from
a crate graph whether a crate is published or must build with stable Rust, and a published
crate must. A workspace built only by blaze turns it on (`internal = "warn"` or
`nll_workaround = "warn"` under `[workspace.metadata.blaze.lints]`); blaze's own does. Under
`BLAZE_BORROWCK=nll` it never fires, and under `BLAZE_BORROWCK=off` neither lint does.

**Allowing it.** `#[expect(blaze::nll_workaround, reason = "builds with stable Rust")]`.

## `nopanic`, `noalloc`, `nonblocking`

**What they catch.** A function marked `#[blaze::nopanic]`, `#[blaze::noalloc]` or
`#[blaze::nonblocking]` (blaze extensions, opt-in; they change diagnostics, never code), or a
method of an impl of a trait so marked, that may, through any call:

- `nopanic`: panic or abort: a panic entry point, an `Assert` the MIR keeps (a bounds check, a
  division check; an overflow check when the crate is built with `overflow-checks`, which the
  lint says), a foreign function whose ABI may unwind. An assert the constants decide
  (`arr[2]` on a `[T; 4]`) is not one.
- `noalloc`: call the global allocator (`Vec::new`, `Box::new`, `to_owned`, `malloc`). An
  allocator the code names (`Vec::new_in(frame)`) is allowed.
- `nonblocking`: block the thread: std's and `parking_lot`'s locks, condvars and channels,
  `thread::sleep`, `JoinHandle::join`, and the syscalls that wait (`read`, `write`, `futex`,
  `poll`, `accept`, `connect`).

Each also refuses what it can't see: a call through a `dyn` or a function pointer it can't
follow, and (`noalloc`, `nonblocking`) a foreign function or `asm!` it knows nothing of. The
finding says the shortest call chain to what does it, each step with its file and line. They
are `deny` by default: the function asked for it. The tables and rules are in
[v2/plan/lanes/effects.md](v2/plan/design/effects.md).

**Why it matters.** The promise holds for everything the function reaches, in every crate, and
keeps holding as the code changes: a pool thread's pick never waits, a hot loop never touches
the global heap, a function called from a panic handler or a signal can't panic.

**How to fix it.** Take the panic, allocation or wait out of the path (`get` or `checked_*`,
the task's frame, the scheduler's wait), or mark the callee whose promise you have checked by
hand `#[blaze::nopanic(trusted = "why")]` (likewise the others); the reason is kept in the
proof. Or take the attribute off.

**Allowing it.** `#[expect(blaze::nopanic, reason = "..")]` on the function.

These three are the old spellings of `#[blaze::may(..)]`, next.

## `undeclared_effect`, `unneeded_effect`, `negative_effect`

`#[blaze::may(panic, alloc, block, diverge)]` (a blaze extension, opt-in; it changes
diagnostics, never code) lists what a function, and everything it calls, may do: panic or
abort, call the global allocator, block the thread, not return. A word the list leaves out is
an effect the function does not have. The design, the rules and the tables are in
[v2/plan/design/effects.md](v2/plan/design/effects.md).

**What they catch.**

- `undeclared_effect` (`deny`): a function with an effect its allowance leaves out. The
  allowance is the function's own list; else the list of the trait's method it implements (or
  of the trait); else the `#![blaze::may(..)]` of the nearest module around it; else nothing
  at all in a crate with `#![blaze::effects]`. The first written holds alone: a function's own
  list replaces a module's default, it is not added to it. A function none of these reach is
  not looked at, so a crate that declares nothing is unaffected. The finding says each effect
  with its witness: the shortest call chain to what has it, each step with its file and line,
  or for `diverge` the line of a loop not known to end, or the chain of a recursion and which
  function is called again. Also: an impl's method whose list has a word its trait's method
  does not allow (an impl may declare fewer effects, never more), a function with both a list
  and an old spelling, and a list that is not words separated by commas.
- `unneeded_effect` (`warn`): a word of a private function's list for an effect the function
  is proved not to have. Not on a function with a visibility written, a trait's method or an
  impl's method of one: a public signature may reserve the right, and a trait method's list is
  the bound of its impls. `panic` is found only where the crate is checked with overflow
  checks, so the shorter list holds in dev and in release.
- `negative_effect` (`warn`): `#[blaze::nopanic]`, `#[blaze::noalloc]` or `#[blaze::nonblocking]`
  on a function under a module's default or `#![blaze::effects]`. The old spellings still mean
  what they meant, everywhere; where effects are declared, the function says what it may do.

`diverge` in short: a counted `while i < n { ..; i += 1; }` is known to end (with the crate's
overflow checks, any positive step and `<=` too); every other loop (`for`, `while let`,
`loop`), recursion, and a call through a function pointer or a `dyn` whose trait method has no
list need the word, until `decreases` clauses exist. What the check can't see (an unknown
foreign function, a function pointer or unlisted `dyn` parameter) may do anything, so it needs
every word.

**Why it matters.** A signature that lists what can happen is complete: a reader, and a caller
on a pool thread, in a signal handler or under a deadline, knows what the call can do without
reading its body, and the list keeps holding as the code changes, in every crate it reaches.

**How to fix it.** `blaze check --fix` writes each list: the missing words added
(`undeclared_effect`; under `#![blaze::effects]` that is a `#[blaze::may(..)]` with exactly the
inferred effects on every function that needs one), the unneeded word taken off
(`unneeded_effect`), the old spellings rewritten to the list they stand for
(`negative_effect`: `nopanic` and `nonblocking` on a function that allocates and may not return
become `#[blaze::may(alloc, diverge)]`). Or handle the effect so the function no longer has it
(`get` or `checked_*`, the task's frame, the scheduler's wait, a counted loop), or mark a
callee proven by hand `#[blaze::may(.., trusted = "why")]`: its list is then taken at its word,
and the reason is kept in the proof.

**Allowing it.** `#[expect(blaze::undeclared_effect, reason = "..")]` (likewise the others) on
the function.

## `pure`

**What it catches.** A function marked `#[blaze::pure]` (a blaze extension, opt-in; it changes
diagnostics, never code), or a method of an impl of a trait so marked, whose result may depend
on more than its arguments, through any call: reading the environment, the clock, files or
stdio; spawning, sleeping or asking about threads; reading or writing a `static mut`, a static
with interior mutability or a thread-local; observing an address (a pointer cast to a number, a
pointer hashed, ordered or formatted with `{:p}`); `RandomState` or the system's randomness; a
foreign function or `asm!` not known to be deterministic. A root parameter the check can't see
through is refused too: a `dyn` or a function pointer (take a type parameter), a raw pointer, or
state with interior mutability behind a shared reference (`&AtomicU64`, `&Mutex<T>`), which
another thread can change during the call. Allocation, panics, atomics and cells on memory the
function owns are allowed. The finding says the shortest call chain to what does it, each step
with its file and line. It is `deny` by default: the function asked for it. What counts and why
is in [v2/plan/lanes/pure-fns.md](v2/plan/design/pure-fns.md).

**Why it matters.** A pure function's result can be kept and reused by its arguments (the
compute kernel's memoized steps), and two calls with the same arguments can be merged; that is
only right when the promise holds for everything the function reaches, in every crate.

**How to fix it.** Pass what the function reads in as an argument, or mark the callee whose
promise you have checked by hand `#[blaze::pure(trusted = "why")]` (a function, a foreign
function, or a trait: every impl's method and every call through `dyn` of it); the reason and
the trusted function's crate are kept in the proof. Or take the attribute off.

**Allowing it.** `#[expect(blaze::pure, reason = "..")]` on the function.

## `outline_generic_body`

**What it catches.** A generic function whose type parameters only feed a conversion at entry
(`p: impl AsRef<Path>` used once as `p.as_ref()`, `s: impl Into<String>` used once as
`s.into()`, ...), while the rest of its body, over 30 expressions
(`outline-generic-body-threshold`), uses only what the conversions made.

**Why it matters.** Each instantiation copies the whole body into the binary and compiles it
again, though only the conversion differs.

**How to fix it.** Convert in the generic shell and call a non-generic inner function with the
result, so the body is compiled once (the "momo" pattern). `blaze check --fix` does this for a
free function whose signature allows it.

**Allowing it.** `#[expect(blaze::outline_generic_body, reason = "..")]`.

## `packable`

In `blaze::pedantic_perf`, allowed by default.

**What it catches.** A struct of the crate, with no `#[repr(..)]` and no `#[blaze::bits]`,
whose fields need far fewer bits than its layout gives them, held many at a time: it is the
element of a `Vec`, `VecDeque`, array, slice or `ArrayVec`, or the value of a map (`HashMap`,
`BTreeMap`, hashbrown's, indexmap's or papaya's), as the type of a field or of any expression
in the crate. A `bool` needs 1 bit; a fieldless enum the bits that hold its discriminants
(⌈log2 n⌉ for the default `0..n`, its full size with an integer `#[repr]`); a field that is
such a struct of the crate itself what its fields need; anything else its full size. The packed
size is those bits in whole bytes, rounded up to the largest alignment of a field kept at full
width. It is found when packing at least halves the struct, or brings it from over a cache line
(64 bytes) to one or less. The message names the struct, its size, the bits it needs, the
packed size and the first collection that holds it (file and line); a note lists the narrow
fields. With a [profile](#a-profile) configured, a note says whether a function that stores it
is hot, and how hot. A struct `enum_vec_padded` finds a field of is left to that lint.

**Why it matters.** Every element of the collection pays for the bytes its flags don't use: a
struct of five `bool`s and a four-variant enum takes 6 bytes and needs 7 bits, so a `Vec` of
them is six times the memory and the cache misses it could be. The idea is Zig's packed
structs, which store each field at its bit width in one integer.

**How to fix it.** Mark the struct `#[blaze::bits]` (an opt-in blaze extension: each `bool`,
enum and small-integer field is stored at its bit width), or gather its `bool`s into one
`bitflags` set. There is no automatic fix: a packed field has no address, so first check that
nothing borrows a field (`&s.f`, `&mut s.f`), and each access becomes a shift and a mask.

**Not found.** Collections of the struct in other crates of the workspace (the crate that
defines it is searched); integer fields count at their full width until blaze has
narrower integer types; an enum with explicit discriminants that don't evaluate counts at its
full size.

**Why it is off.** Packing trades a shift and mask per access, and fields that can't be
borrowed, for memory; `#[blaze::bits]` only builds under blaze.

**Allowing it.** `#[expect(blaze::packable, reason = "..")]` on the struct.

## `panic_in_drop`

**What it catches.** A `Drop::drop` whose body can panic: `unwrap`, `expect`, `unwrap_err` or
`expect_err` of an `Option` or `Result`; `panic!`, `assert!`, `unreachable!`, `todo!`,
`unimplemented!`; `panic_any` or `resume_unwind`; an index; an integer `/` or `%` by a divisor
that isn't a nonzero literal.

**Why it matters.** `drop` also runs while a panic unwinds the stack. A second panic then aborts
the process, and only the first panic's message is left. Outside unwinding, a panicking `drop`
leaks whatever the rest of it would have released.

**How to fix it.** Handle the error inside `drop` (log it, or ignore it), or give the type an
explicit `close` or `finish` method that returns the error and have `drop` do the best-effort
part. To keep a check that should only run outside unwinding, guard it with
`if !std::thread::panicking() { .. }`.

**Not found.** Panics in functions `drop` calls (only its own body is read), and arithmetic
overflow. A `drop` that calls `std::thread::panicking()` anywhere is not checked.

**Allowing it.** `#[expect(blaze::panic_in_drop, reason = "..")]` on the `drop` function.

## `pod_serialized_by_field`

In `blaze::pedantic_perf`, allowed by default.

**What it catches.** A `#[repr(C)]` struct whose every field is an integer or a float (or an
array of them; no `bool` or `char`), with no padding, that implements serde's `Serialize` or
`Deserialize`.

**Why it matters.** Its bytes already are a fixed layout, yet serde visits every field of every
value through the format's code. Goose keeps nested data inline and moves it as bytes.

**How to fix it.** `#[derive(bytemuck::Pod, bytemuck::Zeroable)]` (or zerocopy's `IntoBytes` /
`FromBytes`) and write or read a whole slice of them as its bytes in one copy. That writes the
machine's byte order.

**Why it is off.** The lint can't see whether the values are serialized often enough to
matter, nor whether the format needs serde's (text, or a fixed byte order).

## `profile_hot`

**What it catches.** Nothing of its own: it is the level of the perf lints in a function the
configured profile ([A profile](#a-profile)) shows hot. In such a function the hot lints
(`hot_alloc`, `hot_alloc_call`, `hot_clone`, `hot_dyn`), which are `allow` outside code marked
hot, run at `profile_hot`'s level, as if the function were `#[warn(blaze::hot)]`; and a finding
of a general perf lint (`bounds_check_in_loop`, `collect_then_consume`, `hash_twice`,
`unbuffered_io`, `fmt_allocates`, ...) there says that the function is hot per the profile and
by how much. A level set for the lint itself, by an attribute or a flag, still wins.

**Why it matters.** A hot lint is worth fixing where the time goes. Marking code hot by hand
goes stale; a profile says where the time went in the run it was taken of.

**How to fix it.** Fix what the lint found: the note `hot per profile <file>: N% of its counts`
says how much of the profile the function is.

**Allowing it.** `#[allow(blaze::profile_hot)]` on the function or module, or `profile_hot =
"allow"` in `[workspace.metadata.blaze.lints]`, keeps the hot lints at their own levels.

## `push_without_reserve`

**What it catches.** A `Vec` or `String` made empty (`new()`, `default()`, `vec![]`) whose next
use is a `for` loop over a range or an `ExactSizeIterator` that pushes to it exactly once per
iteration (a top-level statement, no `break`, `continue` or `return`). A `let` inside another
loop is left to `scratch_in_loop` when that is on; clippy's `vec_init_then_push` finds pushes
right after the `let`, never a loop.

**Why it matters.** The buffer reallocates and copies about log2(n) times as it grows, though
the count was known before the loop started. Goose grows a bump region in place for the same
reason.

**How to fix it.** `Vec::with_capacity(n)` (`String::with_capacity`, in bytes: a lower bound).

**Allowing it.** `#[expect(blaze::push_without_reserve, reason = "..")]` on the function.

## `relaxed_publish`

**What it catches.** An `AtomicBool` or `AtomicPtr` (a struct field or a `static`) that one
function sets with a `Relaxed` `store` or `swap`, while another function of the crate loads it to
decide whether to read other data: the load is an `if` condition (also under `!`, `&&`, `||`)
whose branch reads a field, a dereference or an index, or the pointer loaded is dereferenced
(`*p`, `p.as_ref()`, `p.read()`). Both sites are reported, each naming the other function.

**Why it matters.** `Relaxed` orders nothing but the atomic itself. A thread that sees the flag
set may still see the data from before the writer wrote it, on ARM and other weakly ordered
processors, and the compiler may reorder the writes on any target. The bug shows up rarely and
only under load.

**How to fix it.** Publish with `Ordering::Release` (on the `store` or `swap`) and read with
`Ordering::Acquire` (on the `load`), so the data written before the store is visible after the
load. If the flag really guards nothing, say so with a `// relaxed: <why>` comment on the
atomic's declaration or on the store's line.

**Not found.** Integer atomics (often counters), `compare_exchange` and `fetch_*` used as the
publish, and a guard that reads data through a function it calls.

**Allowing it.** The comment, or `#[expect(blaze::relaxed_publish, reason = "..")]`.

## `scratch_in_loop`

**What it catches.** A `Vec`, `String` or hash map (std's or hashbrown's) made inside a loop's
body (`new()`, `default()`, `with_capacity(..)`), grown there, and only borrowed, so it is
dropped at the end of every round. One whose type holds a reference is left alone (hoisted, it
would outlive what each round borrows).

**Why it matters.** Every round allocates a fresh buffer, grows it from nothing and frees it.
Goose keeps such scratch on a data stack that each round resets.

**How to fix it.** Make it once before the loop and call `.clear()` at the start of each round:
it keeps its capacity, so after the first rounds nothing allocates.

**Not found.** A buffer built in an iterator closure (`map(|x| ..)`).

**Allowing it.** `#[expect(blaze::scratch_in_loop, reason = "..")]` on the function.

## `seqcst_unexplained`

**What it catches.** `Ordering::SeqCst` on an atomic operation with no `// seqcst: <why>` comment
on its line or the line above.

**Why it matters.** `SeqCst` is rarely what an algorithm needs, costs a full fence on some
targets, and hides which ordering the code relies on.

**How to fix it.** Take the weaker ordering the operation needs (a load `Acquire`, a store
`Release`, a read-modify-write `AcqRel`), or say why it needs one total order with
`// seqcst: <why>`.

**Allowing it.** The comment, or `#[expect(blaze::seqcst_unexplained, reason = "..")]`.

## `small_map`

**What it catches.** A hash map or set, or a `BTreeMap`/`BTreeSet`, that provably holds at most 8
entries (`small-map-max-entries`), each under a key known at compile time (a literal, a `const`,
a unit enum variant):

- a local built empty (`new()`, `with_capacity(n)` with a literal `n` up to the limit) or from a
  literal array (`HashMap::from([..])`, `[..].into_iter().collect()`), then only read (`get`,
  `contains_key`, `len`, `iter`, ...) and given constant-key `insert`s that run once (not in a
  loop);
- a private field whose struct literals build it that way, that the crate only reads, compares,
  clones, formats or inserts constant keys into, with at most 8 distinct keys across the crate.

**Why it matters.** Hashing a key and probing a table costs more than comparing the key with a
few others, and the table is a heap allocation.

**How to fix it.** Keep the entries in a sorted array (`[(K, V); N]`) and look up with
`binary_search_by_key`, or scan it with `iter().find(..)`; when every key is a constant, a `match`
on the key needs no table at all.

**Not found.** A map whose keys come from input, one passed to a function or returned (so a
public API never sees its type change), one extended in a loop, and concurrent maps.

**Allowing it.** `#[expect(blaze::small_map, reason = "..")]`.

## `soa_candidate`

In `blaze::pedantic_perf`, allowed by default.

**What it catches.** A struct of the crate, of at least 32 bytes, held in a `Vec`, `VecDeque`,
slice or array, whose loops over that collection touch at most a quarter of its bytes. A loop
over the collection is read from MIR, one per body:

- a `for` loop whose element is the struct (by value or reference) and whose iterable holds it:
  the collection, or std's iterator over one (`v.iter()`, `v.iter_mut()`, `v.drain(..)`);
- the closures given to iterator methods (core's `Iterator`, rayon's `ParallelIterator`) along
  one chain, `v.iter().filter(..).map(..)`, whose parameter is an element; a `for` loop over a
  chain is the same loop as its closures;
- a loop or iterator closure that indexes a collection of the struct (`v[i].f`).

What a loop touches is the fields of the element it reads or writes, through copies and
reborrows of the element. An element used whole (passed to a function or a method, copied into
another value, captured, returned) touches all of it, and then nothing is found. Without a
[profile](#a-profile) it takes at least two such loops; with one, the loops counted are those
in functions the profile shows hot, and one is enough. The message names the struct, its size,
the fields the loops touch and the bytes of each element they need; a note per loop gives its
function, file and line and the fields it touches (and, with a profile, its share of the
counts). Loops in `#[test]` functions are not counted.

**Why it matters.** A loop over a `Vec<T>` brings whole elements into the cache. A loop that
reads one `u32` of a 64-byte element uses 4 of every 64 bytes it loads, so it is bound by
memory bandwidth at a sixteenth of the speed the same loop over a `Vec<u32>` runs at, and it
can't be vectorized over the field. Stored as columns, a struct of arrays, each loop reads only
the fields it needs, packed one after another.

**How to fix it.** Store the collection as columns with a struct-of-arrays derive. We recommend
[`soa-rs`](https://github.com/tim-harding/soa-rs) (1.0, 2026): `#[derive(Soars)]` on the struct
and `Soa<T>` in place of `Vec<T>`, one allocation with a slice per field (`soa.a()`), indexing
and iteration that hand out a reference per field. The older and more widely used
[`soa_derive`](https://github.com/lumol-org/soa-derive) (`#[derive(StructOfArray)]`, which
generates a `TVec` of one `Vec` per field) does the same with one allocation per field. Without
a crate, split the fields the hot loops touch into a struct of their own, kept in a `Vec` beside
one of the rest, indexed alike. There is no automatic fix: every use of the collection changes.

Jai showed struct-of-arrays as a language feature (a `SOA` keyword on a type) in its early
demos and later took it out of the language; Zig has it as a library type,
`std.MultiArrayList`, built on comptime reflection over the struct's fields. Rust's derives are
the same kind of library, so blaze offers a lint that says where the layout pays, not a new
language feature.

**Not found.** Collections of the struct in other crates (the crate that defines it is read);
a method called on the element counts as using all of it, as its body is not followed; a closure
parameter that is a tuple (`zip`, `enumerate`) is not taken as an element; drops are not
counted. A body whose MIR can't be built is named in a note, and its loops are not counted.

**Why it is off.** Columns cost every access that wants the whole element (a row is gathered
from as many places as it has fields), and the change reaches every use of the collection.
Without a profile it can't tell a hot loop from one that runs once.

**Allowing it.** `#[expect(blaze::soa_candidate, reason = "..")]` on the struct.

## `stable_sort_needless`

**What it catches.** `sort()` of a slice or `Vec` whose elements can't be told apart when they
compare equal: `String`, tuples and arrays of such, and structs and enums that derive
`PartialEq`, `Eq`, `PartialOrd` and `Ord` over fields that are all such. Also
`sort_by_key(|x| *x)` (`x.clone()`, `x`) and `sort_by(|a, b| a.cmp(b))` over such elements,
integers and other primitives included. `sort()` of primitives is clippy's
`stable_sort_primitive` and is left to it.

**Why it matters.** A stable sort keeps equal elements in their old order, which costs a buffer
of half the slice and more moves. When equal elements are identical, the unstable sort gives
the same result in place, faster.

**How to fix it.** `sort_unstable()`, `sort_unstable_by(..)`, `sort_unstable_by_key(..)`.

**Not found.** A key that is only part of the element (`sort_by_key(|x| x.id)`), a hand-written
`Ord`, floats, and references other than `&str`: there equal elements can differ, and which
comes first (for example, which one a `dedup` keeps) is what stability decides.

**Allowing it.** `#[expect(blaze::stable_sort_needless, reason = "..")]`.

## `stale_hot_marker`

**What it catches.** A function marked hot with a lint attribute that turns `blaze::hot` or one
of its lints on (`#[deny(blaze::hot)]`, `#[warn(blaze::hot_alloc)]`) that the configured profile
([A profile](#a-profile)) counts as cold: below `profile-cold-percent` of its counts, or none.

**Why it matters.** A hot marker holds the function to the hot lints and tells readers its
speed matters; on code that does not run, both spend effort where no time goes.

**How to fix it.** Take the marker off, or name a profile of the workload the marker is for. A
count of none can also mean every call was inlined before the profile was taken (LLVM's
pre-inliner under `-C profile-generate`), and the count is in the callers: the message says so.
For that reason `#[inline]` and `#[inline(always)]` are not taken as hot markers.

**Allowing it.** `#[expect(blaze::stale_hot_marker, reason = "..")]`.

## `stdout_lock_in_loop`

**What it catches.** `println!`, `print!`, `eprintln!`, `eprint!`, or a `write` on an unlocked
`stdout()` or `stderr()`, inside a loop. One finding per loop, at its first call, with a count
of the rest.

**Why it matters.** Each call takes the stream's lock and writes on its own; `stdout` is also
line-buffered, so every line is a system call.

**How to fix it.** Lock once before the loop and write to the lock, buffered for many lines:

```rust
use std::io::Write;
let mut out = std::io::BufWriter::new(std::io::stdout().lock());
for x in xs {
    writeln!(out, "{x}")?;
}
out.flush()?;
```

**Allowing it.** `#[expect(blaze::stdout_lock_in_loop, reason = "..")]`, for a loop that runs
a few times.

## `unbounded_from_input`

**What it catches.** A loop that reads input and grows a collection or an unbounded channel once
per round, with nothing in the loop that checks or limits the size. The loop reads input when it
is a `for` over std's `io::Lines`, `io::Bytes`, `io::Split` or `net::Incoming`, or its body calls
a std `io` or `net` read (`read*`, `recv*`, `accept`). Growing is `push`, `push_back`,
`push_front`, `insert`, `extend` or `extend_from_slice` on a `Vec`, `VecDeque`, `HashMap` or
`HashSet` (std's or hashbrown's) that lives past one round, or a `send` on std's
`mpsc::Sender`, tokio's `UnboundedSender`, or a crossbeam `Sender` made by `unbounded()` in the
same function.

**Why it matters.** Whoever writes the input decides how much memory the program uses: a peer
that sends faster than the program drains, or input that never ends, grows the collection until
the process runs out of memory.

**How to fix it.** Cap it in the loop (check `len()` and stop, drop or flush past a limit), or use
a bounded channel (`sync_channel`, `crossbeam_channel::bounded`, tokio's `channel(n)`) so a slow
receiver pushes back on the reader. A call of `len`, `capacity`, `is_full`, `is_empty`, `clear`,
`truncate`, `pop*`, `remove`, `drain`, `retain`, `try_reserve` or `shrink_to` on the same
collection in the loop counts as a cap.

**Not found.** Input read by a non-std function the loop calls, and a cap checked outside the
loop or by a helper.

**Allowing it.** `#[expect(blaze::unbounded_from_input, reason = "..")]`.

## `unbuffered_io`

**What it catches.** A `std::fs::File`, `TcpStream`, `UnixStream` or child-process pipe
written (`write`, `write_all`, `write!`) at least 2 times in one function
(`unbuffered-io-writes`), or inside a loop, or read (`read`, `read_exact`) into an array of at
most 64 bytes (`unbuffered-io-small-read`) the same way, with no `BufWriter` or `BufReader`. A
bare `write` in a loop is taken for a partial-write retry and left alone, and `stdout` in a loop
is `stdout_lock_in_loop`'s.

**Why it matters.** Each call on a raw file or socket is a system call, thousands of times the
cost of copying the same bytes into a buffer.

**How to fix it.** Wrap the stream once in `std::io::BufWriter::new(..)` (`BufReader` for reads)
and make the calls on the wrapper. A `BufWriter` writes what is left when dropped; call
`flush()` to see the error that drop ignores.

**Allowing it.** `#[expect(blaze::unbuffered_io, reason = "..")]`, where each call must reach
the system on its own (a log that must survive a crash).

## `unwind_across_ffi`

**What it catches.** A function written in Rust with a foreign ABI that can't unwind
(`extern "C"`, `extern "system"`, `extern "sysv64"` and the rest, but none of the `-unwind`
forms), whose body can panic (the same panics `panic_in_drop` counts) and never calls
`std::panic::catch_unwind`.

**Why it matters.** A panic can't unwind out of such a function: the process aborts at its
boundary (before Rust 1.81 it was undefined behavior). The foreign caller never gets an error it
could handle.

**How to fix it.** Wrap the body in `std::panic::catch_unwind` and turn a panic into an error
code the caller understands, or, when the caller is built to unwind, declare the function with
the `-unwind` form of its ABI (`extern "C-unwind"`).

**Not found.** Arithmetic overflow (it panics only with overflow checks on), and panics in
functions the body calls. A body that calls `catch_unwind` anywhere is taken as catching all of
its panics.

**Allowing it.** `#[expect(blaze::unwind_across_ffi, reason = "..")]` on the function.

## `vecreport`

In `blaze::pedantic_perf`, allowed by default. Unlike the others it is not run by `blaze
check`: the release build reports it, from the code LLVM made. Turn it on for a build with
`RUSTFLAGS="-W blaze::vecreport"` (or `-D`, `--force-warn`, or the group); `-A` turns it off
again, the last flag winning.

**What it catches.** What the optimized code of the workspace's functions still does inside a
loop, as LLVM leaves it once every pass has run:

- a branch out of a loop to a path that panics: a bounds check, an overflow check, a
  division-by-zero check or an `unwrap` the optimizer could not prove dead, named by the panic
  function its path calls (`panic_bounds_check`, `panic_const_add_overflow`, ..);
- a loop the loop vectorizer left scalar, with what it said and why (its missed and analysis
  remarks: a call it can't vectorize, a dependence it can't rule out, a loop count it can't
  compute).

Each finding is a warning at its source line (`src/lib.rs:12:9: a bounds check survived
optimization inside this loop: its path calls `core::panicking::panic_bounds_check``), never
an IR dump, sorted by file, line, column and message; the same finding in two instances of a
generic function is printed once. Built with a profile (`-C profile-use`), only loops the
profile's summary counts as hot are reported, with their count; a loop no count reaches is not.

It reads what [`bounds_check_in_loop`](#bounds_check_in_loop) predicts from MIR: that lint
says a check may run every iteration, this one says whether LLVM kept it.

**Why it matters.** A check kept in a loop costs a compare and branch each iteration and, worse,
keeps the loop from vectorizing: a panic path is a side exit the vectorizer can't move past.

**How to fix it.** As for `bounds_check_in_loop`: prove the range before the loop
(`assert!(n <= v.len())`, a reslice `let v = &v[..n];`), iterate instead of indexing, use
`get`, `checked_*` or `wrapping_*`. For a loop left scalar, what LLVM says is the fix's start:
an opaque call kept in the body, a possible alias between two slices, a reduction over floats
(which LLVM won't reorder without `fast-math`).

**Not found.** Code built by the stencil tier or Cranelift (dev builds): a stencil keeps every
check its MIR has and vectorizes nothing, which [`checkfree`](#checkfree)'s MIR scan already
lists. Code of dependencies and the standard library. A `#![warn(blaze::vecreport)]` in the
source: the report is part of the build's keys, which are known before its source is read, so
only the flags turn it on.

**Allowing it.** Leave it off, or `-A blaze::vecreport` after a flag that turned it on.

## `wide_index`

**What it catches.** A non-`pub` field of type `usize` or `u64` whose every read in the crate is
an index into one container field (`self.items[self.at]`, `arena.nodes.get(node.next)`, through
an `as usize` cast), otherwise only assigned. A field that is also a count, a length or an
argument is never found.

**Why it matters.** On 64-bit targets a `u32` index halves the link: more links per cache line,
smaller nodes. Goose's links are narrow and relative for the same reason.

**How to fix it.** Store a `u32` (a newtype, so it can't be mixed up with another index) and
widen it with `as usize` where it indexes; check the length where the container grows.

**Allowing it.** `#[expect(blaze::wide_index, reason = "..")]` on the struct.
