---
status: current
summary: The general `blaze::` lints `blaze check` runs in every workspace (`blaze::general`): what each catches, why it costs, how to fix or allow it, and the `clippy.toml` keys that tune them; then the three guard lints of `blaze::internal` that ask for a stated reason on the line (`durable_write`, `unbounded_wait`, `combined_identity`), and `implicit_allocator`, which no comment keeps: don't allocate where a borrow or a buffer does, else name the allocator, `Global` included, with a fix to the explicit form; the gate counts the `// alloc:` lines left to convert and the explicit `Global` uses.
---
# The general lints

`blaze check` runs clippy's lints and lints of its own, named `blaze::`. The lints of the group
`blaze::general` look only at the standard library and widely used crates (`Arc`, `HashMap`,
`BTreeMap`, `Vec`, `String`, atomics, `Result`, `Drop`, std's IO, arc-swap, crossbeam,
hashbrown, tokio, the common fixed-seed hashers, `core::fmt`, iterators, slices' sorts), so they are useful in any workspace. They are on at `warn` by default, everywhere.

The other `blaze::` lints (the group `blaze::internal`) check conventions of blaze's own code
base; they are allowed unless a workspace turns them on.

The group `blaze::pedantic_perf` (`deep_drop_at_exit`, `loop_match_candidate`, `packable`,
`pod_serialized_by_field`, `soa_candidate`, `vecreport`) is speed advice that trades something else away or can't see whether the code is hot; it is
allowed unless a workspace or crate turns it on (`pedantic_perf = "warn"`; `vecreport`, which
the release build reports, only by its flags). `checkfree`,
`nopanic`, `noalloc`, `nonblocking`, `undeclared_effect` and `unfulfilled_effect` are in
`blaze::general` but `deny` by default, and fire only where the code asks: on functions marked
with their attribute (`#[blaze::checkfree]`, `#[blaze::nopanic]`, ...), for `undeclared_effect`
where a `#[blaze::expect(..)]` or `#![blaze::effects]` reaches the function, and for
`unfulfilled_effect` on a function's own `#[blaze::expect(..)]`.

The group `blaze::edition` flags what the blaze edition removes
([the plan](v2/plan/design/edition.md)). Its lints here are those of code blaze cannot analyse:
a procedural macro a crate of the workspace defines ([`proc_macro`](#proc_macro), one lint per
kind, the three also a group of their own, `blaze::proc_macro`) and a build script that uses
the standard library's ambient IO ([`build_script_ambient`](#build_script_ambient)); and the
lint of what blaze's own standard library replaces, [`std_use`](#std_use). Each tells
code to move to something of blaze's, so none says "write X" before X is in the tree: until
then each is `allow` by default and says in its help that the replacement is planned. The
switches are the constants of `crates/blaze-lint-defs/src/ready.rs`; a test holds this
table to the levels the lints have:

| lint | default | the replacement it names | warns by default once |
|---|---|---|---|
| `blaze::proc_macro_derive` | `allow` | a generic impl over the type's fields (`for const f in fields_of::<T>()`) | `REFLECTION_REPLACES_MACROS` is `true`: phase 2 of [compile-time reflection](v2/plan/design/comptime.md) is built and its tests pass |
| `blaze::proc_macro_attribute` | `allow` | reflection over the item and a generic function | the same switch |
| `blaze::proc_macro_fn_like` | `warn` | a `const fn` or a `const` block; a macro that parses a foreign syntax may stay | now: `const fn` is Rust's |
| `blaze::build_script_ambient` | `allow` | a capability the script is handed (`Fs`, `Env`, `Process`, `Net`, `Clock`) | `CAPABILITIES_FOR_BUILD_SCRIPTS` is `true`: [blaze's standard library](v2/plan/design/std.md) is in the tree and a build script is handed its capabilities |
| `blaze::not_latest_edition` | `allow` | `edition = "blaze-2026.11"` in the package's manifest | `EDITION_SHIPS` is `true`: bstd ships with blaze, so a package of [the edition](v2/plan/design/std.md#the-blaze-edition-opting-in) builds wherever blaze runs |
| `blaze::std_use` | `allow` | what [bstd](v2/plan/design/std.md) has in place of each item of Rust's `std`, by [its table](#std_use) | never by default: a crate asks with `#![blaze::capabilities]`, and a workspace with a line of its `lints-when-ready` table, which holds once `BSTD_IN_TREE` is `true` |

`edition = "warn"` (or `#![warn(blaze::edition)]`) turns the whole group on whatever the
switches say; `proc_macro = "allow"` (or `#![allow(blaze::proc_macro)]`) turns the three macro lints
off. They look only at the workspace's own crates, as every lint here does: `blaze check` lints
the workspace's packages, and a registry or git crate is checked, not linted, its lints capped
at `allow`. The macro and build-script lints have no fix: neither rewrite is mechanical.
`std_use` has one for its renames alone.

A workspace can wait for a switch too. Beside `[workspace.metadata.blaze.lints]`, the table
`[workspace.metadata.blaze.lints-when-ready]` takes the same lines, of lints that have a
switch: each sets its level once the lint's switch is `true`, and nothing until then. It is a
table of its own because a released blaze refuses a lint name it does not know in the first
table, and reads no table it does not know: the line can be written before the blaze that
builds the tree has the lint. blaze's own manifest does not wait for `BSTD_IN_TREE`: its
ratchet denies `std_use` now ([the ratchet](#workspace-levels-and-the-ratchet)).

The group also holds what the edition removes or respells (`async_fn`, `global_state`,
`lossy_as`, `mut_ref`), what it writes at a crate's border (`unmarked_pure`,
`unmarked_nopanic`, `unmarked_noalloc`, `unmarked_nonblocking`) and the type facts
(`program_lifetime`, which is `deny` where it fires; `could_be_program_lifetime` and
`could_be_ref32` are [`blaze::suggest`](#suggestions)'s). None of these has a switch: each is allowed unless a workspace or crate
turns it, or the group, on. Of them only `mut_ref` and the four `unmarked_*` lints have a fix.
The three `unmarked_*` effect lints still write the old spellings (`#[blaze::nopanic]`, ..),
which [`negative_effect`](#undeclared_effect-unfulfilled_effect-negative_effect) then asks to
respell as `#[blaze::expect(..)]`; they are to be ported to write the list.

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

## Workspace levels and the ratchet

blaze's own workspace denies every lint of blaze's in every crate, and lets a crate that
still trips one stay at `warn` on it only while the crate is listed in the lint's baseline.
The levels are in `[workspace.metadata.blaze.lints-ratchet]`, which this blaze reads in place
of `[workspace.metadata.blaze.lints]`: every group at `deny`, and below it, each with its
reason beside it in the manifest, `hot` (the costs that matter only in hot code), the parallel
lints (suggestions) and `vecreport` (not run by `blaze check`). The released blaze that builds
the tree reads the older table, at `warn`.

The table's `baselines` key names the directory, `crates/blaze-lints/baselines`. Its file
`blaze.<lint>.txt` has a line `crate<TAB>count` per crate held at `warn` on that lint, or
`crate<TAB>unmeasured` before a run has counted it; each listed crate gets
`--warn=blaze::<lint>` after the workspace's levels. A crate with no line is at `deny`. The
list only shrinks:

- `harness/v2/guards.py '{"command": "ratchet", "base": "<rev>"}'` refuses a crate a baseline
  did not list at the base, a count that rose, and a count turned back to `unmeasured`. The
  gate's `lint-baselines` job runs it against the pull request's base.
- `blaze check --workspace --target aarch64-unknown-linux-gnu --baselines
  crates/blaze-lints/baselines` fails on a count over its line or a line over its count.
  Adding `--bless-baselines` records each count, drops each crate at zero, and deletes a
  baseline with no crate left, saying so. That lint is then `deny` everywhere.

Two lints are apart from this. `todo` is named `warn` in the ratchet table, so a hole is a
warning in a local check and the gate denies it on its command line
(`blaze check --deny blaze::todo`, [`todo`](#todo)). `unreachable` is never below `deny`: a
line that sets it to `allow` or `warn` in `[workspace.metadata.blaze.lints]`, the ratchet or a
package's `[lints.blaze]`, a baseline `blaze.unreachable.txt`, and `--allow` or `--warn` of it
on the command line are each an error naming it ([`unreachable`](#unreachable)).

A new crate gets no line, so it is held to every lint from the start. Making a lint `warn`
again for a crate is a change to this section's rule, not to a baseline.

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
| `unmarked-effects-private` | `unmarked_pure`, `unmarked_nopanic`, `unmarked_noalloc`, `unmarked_nonblocking` | `false` (a flag; `true` also finds functions with no visibility written) |

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

## `async_fn`

In `blaze::edition`, allowed by default.

**What it catches.** An `async fn`, an `async` block and an `async` closure: one finding per
outermost one, so an `.await`, or an `async` block inside an `async fn`, goes with the finding
of the construct it is in. An `async fn` declared inside another function is found on its own.

**Why it matters.** The blaze edition has no `async` or `.await`: code runs on a fiber, a call
that waits parks the fiber, and the may-suspend effect is inferred from the body. `async`
splits every API into two colours and every caller has to choose one.

**How to fix it.** Write it as a plain function run on a fiber, and call what it awaited
directly.

**Not found.** What a macro wrote with no input of the file (a macro's own `async` block).

**Allowing it.** `#[expect(blaze::async_fn, reason = "..")]` on the function or item.

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

## `not_latest_edition`

A package of the workspace that has not opted into the latest blaze edition
([std.md](v2/plan/design/std.md#the-blaze-edition-opting-in)): one on a Rust edition, or, once
there is more than one, on an older blaze edition. One rule covers both.

```
warning: package `app` is on Rust edition 2024, not the latest blaze edition `blaze-2026.11`, under which `main` takes what it may touch (`fn main(cx: &Cx, io: Io)`) and `std` is bstd, blaze's capability-based standard library: set `edition = "blaze-2026.11"` in its Cargo.toml
```

**Once per package.** The finding is about the package, has no place in a source file, and is
said by the package's library, or by a binary or an example of a package with no library. A
test, a bench, a build script and a proc-macro crate never say it. It is stored with the
crate's other diagnostics and drawn as they are: in `--message-format json` it is a
`compiler-message` whose code is `blaze::not_latest_edition`.

**Never for code that is not yours.** A dependency, a registry or git crate, vendored code
outside the workspace and the standard library are not linted.

**Default.** `allow` until `EDITION_SHIPS` (a package of the edition names bstd by path
until blaze ships it, which only a checkout of blaze can do); `warn` from then on.

**Allowing or denying it.** `not_latest_edition = "allow"` (or `"deny"`, for a CI that holds
a workspace to the edition) under `[workspace.metadata.blaze.lints]`,
`#![allow(blaze::not_latest_edition)]` at the crate root, or `-A`/`-D
blaze::not_latest_edition` in `RUSTFLAGS`. blaze's own workspace allows it while cargo
builds blaze (decision D25).

**No fix.** `blaze fix --edition` is planned.

## `build_script_ambient`

In `blaze::edition`; its default is in [the table above](#the-general-lints).

**What it catches.** A build script of the workspace (`build.rs`, or the file the manifest's
`build =` names) that uses the standard library's ambient authority, reaching by itself for:

- the file system: a function of `std::fs`, or of its `File`, `OpenOptions`, `DirBuilder`,
  `ReadDir` or `DirEntry`;
- the environment: `std::env`'s `var`, `var_os`, `vars`, `vars_os`, `set_var`, `remove_var`,
  `current_dir`, `set_current_dir`, `args`, `args_os`, `current_exe`, `temp_dir`, `home_dir`;
- processes: `std::process`'s `Command` and `Child`, `exit`, `abort`, `id`;
- the network: `std::net`'s `TcpStream`, `TcpListener`, `UdpSocket`;
- the clock: `now` and `elapsed` of `Instant` and `SystemTime`.

It is [`std_use`](#std_use) over a build script: the same table of what bstd replaces and the
same walk, here the rows of these five authorities, reported once per script and not once per
function, since it is the script's whole reach that keeps blaze from analysing it. `std_use`
says nothing of a build script.

A use is a path that names the function, called or not, or a method call (`cmd.status()`),
found by what it resolves to: through a `use`, an alias, a glob or a re-export, never by the
text written. There is one finding per distinct item per script (a free function, or the type
for a type's functions), at its first use, with the count of its uses in the whole script:

```text
warning: `std::env::var`, used 2 times in this build script, reads or changes the environment by itself
  = note: a build script that reaches for the file system, the environment or a process by itself cannot be analysed: blaze must run it in a sandbox, trace what it read, and rerun it whenever anything it might have read changes
  = note: the capability that replaces `std::env::var` is `Env` (`Args` for the command line), of blaze's standard library, which is planned and not in this blaze
  = help: nothing to change yet: the replacement is a capability the script is handed (`Fs`, `Env`, `Process`, `Net`, `Clock` of blaze's standard library, docs/v2/plan/design/std.md), which states in the script's signature what it can touch, lets blaze prove what it reads, and reruns it only when that changes; that library is not in this blaze, and this lint is `allow` by default until it is
```

Once the library is in the tree the second note reads "take `Env` (`Args` for the command
line) as a parameter in place of `std::env::var`" and the help "take the capability as a
parameter instead (..)".

**Why it matters.** blaze cannot tell what such a script depends on without running it. It runs
every build script in a sandbox, traces the files and variables it read, and keys the run by
them ([cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox)); a script that walks
its environment or starts a program is keyed by all of it. A script that is handed a capability
states in its signature what it can touch, so what it reads is proved before it runs and it
reruns only when that changes.

**How to fix it.** Nothing yet, while the lint is `allow` by default: the capabilities are
[bstd's](v2/plan/design/std.md) (`Fs` and a `Dir` of it, `Env` and `Args`, `Process` and the
`Runner` that runs a command in a sandbox, `Net`, `Clock`), which is planned. A script that
only prints constant directives is already read and never run.

**Not found.** `println!("cargo::..")` (and `cargo:..`): that is how a script talks to the
build, and information, not ambient authority. A type only named in a signature,
an item only code a macro wrote uses, and `env!` and `option_env!`, which the compile reads.
The same code in a crate that is not a build script: that is [`std_use`](#std_use). Whether a
script is needed at all is not judged.

**No fix.** Which capability a script is handed, and by whom, is not a mechanical rewrite.

**Allowing it.** `#![allow(blaze::build_script_ambient)]` at the top of the script, or
`#[expect(blaze::build_script_ambient, reason = "..")]` on the function with the first use of
an item, which keeps that item's finding.

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

## `env_read`

In `blaze::internal`; `deny` in blaze's workspace (the `lints-ratchet` table of Cargo.toml).

**What it catches.** A read of the process's environment, `std::env::var`, `var_os`, `vars`
or `vars_os`, in a module that `clippy.toml`'s `env-read-allowed` does not name. The function
is found by what a path or a method call resolves to (through a `use`, an alias or a
re-export), one finding per function per body, at its first use, with its count. A module
named there is allowed itself, not the modules below it.

**Why it matters.** A setting read where it is used is an input no key says (rule
keys-cover-every-input), that a test can set only for the whole process, and that two reads of
one name can default differently (rule no-fallback); `vars` is every such input at once. It
was `.clippy.toml`'s `disallowed-methods` ban, at warn, where about 170 reads stood unremarked.

**How to fix it.** Read the setting once, typed, with no default for a missing value, in a
settings module (`base_db::settings`, `blaze_core::home`, `blaze_std::rt` are the ones named
now), and hand it to the code that needs it. A process that hands its whole environment on (a
client to its daemon, a runner to a child) says so where it reads it.

**The instance.** `std::env::vars_os()` filtered to UTF-8 pairs in three places of
blaze-cargo's command line (cli.rs, cli/clean.rs, cli/own.rs, whose `env()` is the one home)
and two of blaze-thin's main.rs.

**Not found.** A build script (`build_script_ambient` holds it), test code, `env!` and
`option_env!` (the compile reads those, and keys by them).

**Allowing it.** `#[expect(blaze::env_read, reason = "..")]` on the function, saying who
the environment is handed to; or the module's path in `env-read-allowed`.

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

## `global_state`

In `blaze::edition`, allowed by default.

**What it catches.** State of the whole program any code can reach by name: a `static mut`; a
`static` whose type has interior mutability, decided by the type system (its type is not
`Freeze`: an atomic, a `Mutex`, `RwLock`, `OnceLock` or `LazyLock`, a `Cell`, `RefCell` or
`UnsafeCell`, a `blaze_core::lock` lock, or anything holding one); and each item of std's
`thread_local!`. The message lists where in the crate it is used, each body once, in a fixed
order: for a `static mut` the functions that write it and those that only read it, for the
rest those that use it (a use through `&` may write).

**Why it matters.** Every access to a `static mut` is `unsafe`, and nothing orders one
thread's write before another's read. Interior-mutable statics and thread-locals are state
shared by every caller, invisible in any signature; a thread-local's value can change under
code on a fiber that moves between threads. The blaze edition removes all three.

**How to fix it.** Choose the owner from the uses the message lists, and hand the state (or a
handle to it) to the code that uses it as a context. For a `static mut` of an integer, `bool`
or pointer that must stay shared, the message names the atomic that fits.

**Not found.** A `static` of plain data that is not `mut` (`static NAMES: [&str; 2]`), a
`const`, and what a macro wrote with no input of the file.

**Allowing it.** `#[expect(blaze::global_state, reason = "..")]` on the item.

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

## `implicit_overflow`

In `blaze::edition`, allowed by default (docs/v2/plan/design/explicit-overflow.md).

**What it catches.** Integer arithmetic whose meaning depends on the profile, wherever blaze
can't show it fits: `+`, `-`, `*`, `<<`, `>>`, their compound forms, unary `-`, and an `as`
that can lose an integer's value (to a narrower integer, between signed and unsigned, a float
to an integer; `usize` and `isize` count as 32 bits on the target side). Also calls whose
overflow follows the profile (`pow`, `abs`, `next_power_of_two`, integer `Iterator::sum` and
`product`) or always panics (`strict_*`). The reach prover judges each operation from the
checks around it (`i < v.len()`, `b <= a`, a mask, a remainder). The message names the fact
that is missing: `can't show `a + b` fits in `u32` here: nothing bounds `b``.

**Why it matters.** With overflow checks an overflowing `a + b` panics; without them it wraps.
Plain arithmetic says neither, so the profile decides what the code means. The verdict is the
same in both profiles.

**How to fix it.** Say what overflow means: `checked_*` with `?` (or a typed error), `wrapping_*`
or `Wrapping<T>`, `saturating_*`, `overflowing_*`, `T::try_from(x)`. Or add a check blaze can
see that bounds the operands. `blaze check --fix` does the rewrite where it is clear: in a
function returning `Option`, `a + b` becomes `a.checked_add(b)?` and `x as u8` becomes
`u8::try_from(x).ok()?`. In a function returning a `Result` whose error takes a
`TryFromIntError`, a cast becomes `u8::try_from(x)?`.

**Not found.** `/` and `%`, whose panics are the same in every profile. Floats, except a float
cast to an integer. Constants, statics and `const { .. }` blocks, where overflow is a
compile-time error. Test code. Arithmetic that a macro makes up (arithmetic written in a macro
call's arguments is still found).

**Allowing it.** `#[expect(blaze::implicit_overflow, reason = "..")]`.

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

## `as_cast`

In `blaze::edition`, allowed by default; its old name `lossy_as` still works.

**What it catches.** Every `as` written in a body. The message names the two inferred types
and what the cast can lose: high bits, a sign, a fraction, precision, a code point, or a
pointer's provenance. `usize` and `isize` count as 32 to 64 bits.

**Why it matters.** `as` is one keyword for a dozen operations, and the reader can't tell
which without the types. The blaze edition removes it.

**How to fix it.** The machine-applicable fix writes the operation by name, with the same
behaviour: `T::from(x)`, `x.truncate::<T>()`, `cast_signed`, `cast_unsigned`, blaze-cast's
helpers, `p.cast::<U>()` or `core::ptr::from_ref(r)`. A fix that keeps wrapping is what
`truncating_cast` then lists.

**One cast, one finding.** With `implicit_overflow` on, a cast that can lose an integer's value,
which its prover can't show keeps that value, is reported by `implicit_overflow` alone, with
its `try_from` fix. `as_cast` gives way there. `truncating_cast` lists the calls the fix
writes, never an `as`.

**Not found.** A cast a macro writes, and one in a type (an array type's length).

**Allowing it.** `#[expect(blaze::as_cast, reason = "..")]`.

## `many_bools`

Allowed by default; part of getting a crate ready for
[records](v2/plan/design/records.md) (the edition's change 4).

**What it catches.** A call or method call that passes two or more bare `true` or `false`
literals by position: `open(path, true, false)`. Nothing at the call says what each chooses,
and two of them swap without a type error. It reads the call alone, so the callee may be any
crate's. Not counted: a literal inside another argument (`f(g(true), true)` passes one), a call
under `#[cfg(test)]` or `#[test]`, and a call a macro wrote.

**What to do.** Name the choices at the call. Under `#![blaze::records]` the callee takes a
record parameter with defaults, `fn open(path: &Path, opts: { create: bool = false, truncate:
bool = false })`, called as `open(path, { create: true, .. })`; without it, two-variant enums
named for the choices. There is no fix: the names are the author's. To keep a call, say why:
`#[expect(blaze::many_bools, reason = "..")]`.

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

## `mut_ref`

In `blaze::edition`, allowed by default; says nothing in a crate without `#![blaze::uniq]`.

**What it catches.** In a crate that says `#![blaze::uniq]`, a unique reference spelled with
`mut`: `&mut T` and `&'a mut T` in a type, `&mut expr` in an expression, `&mut self` and
`&'a mut self` as a receiver, and `ref mut x` and `&mut pat` in a pattern.

**Why it matters.** Under `#![blaze::uniq]`, `&uniq T` is `&mut T`, spelled for what it
guarantees: no other reference to the place is live while it is. Mutation through it follows
from that. The two spellings are the same type and are accepted for each other, so the lint
only brings a crate to the one spelling the edition uses.

**How to fix it.** `blaze check --fix`: the fix is machine-applicable and only respells `mut`
as `uniq`. An operand that starts with `<`, `::`, `!` (or, in an expression, `.`, `{` or
`#`; in a pattern, `.`) would keep `uniq` a name there, so the fix puts it in parentheses:
`&mut <T>::new()` becomes `&uniq (<T>::new())`.

**The gate.** `#![blaze::uniq]`, written as itself among the crate root's leading inner
attributes (not through `cfg_attr` or a macro), makes `uniq` a keyword in exactly those five
places, told by the token after it: `&uniq;`, `&uniq.x`, `&uniq::X`, `&uniq<T>` and
`&uniq!()` stay a variable, path or macro named `uniq`, and in a pattern `&uniq =`, `&uniq,`
and `&uniq::X` stay a shared reference pattern that binds or names `uniq`. Under the gate
`&uniq (x)`, `&uniq [0]`, `&uniq - 1` and `&uniq * x` are unique borrows of the operand, where
rustc would read a call, an index or arithmetic on `uniq`, and the pattern `&uniq (a, b)` is a
unique reference pattern over a tuple, where rustc would read a tuple struct `uniq`. Types print as `&uniq T` in the crate's diagnostics. Without
the gate, `&uniq T` is the syntax error it is in rustc.

**In macros.** A token is parsed under the gate of the crate whose source wrote it, wherever
a macro carries it: `&uniq x` in `vec![..]` or `assert!(..)`, in a function under `#[test]`,
and in the body of a `macro_rules!` of a crate with the gate, expanded in any crate. What a
crate without the gate writes stays Rust, also when it is handed to such a macro. The lint
finds what a macro call of the file is handed (a call's arguments, an item under an attribute
macro) as it finds the file's other code.

**Not found.** `let mut x` and any `mut` binding without `ref`, `mut self`, `*mut T`,
`&raw mut place`, a `&pin mut pat` reference pattern (which has no `uniq` spelling), a
reference already spelled `uniq`, and what a macro's definition writes (token trees, respelled by
hand).

**Allowing it.** `#[expect(blaze::mut_ref, reason = "..")]` on the function or item.

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
  (`arr[2]` on a `[T; 4]`) is not one. Nor is the library's check of an `unsafe` precondition
  (`core::ub_checks::assert_unsafe_precondition!`, which aborts in a build with debug
  assertions) reached through a safe function of the toolchain: `wrapping_shl`, a slice's
  iterator, `NonNull::new`, `MaybeUninit::zeroed` and `CStr::to_bytes` uphold what they call,
  and the toolchain is trusted for `unsafe`. Calling the `unsafe fn` yourself
  (`unchecked_add`, `NonNull::new_unchecked`, `hint::assert_unchecked`) still counts: the
  precondition is yours to uphold, and the finding names the call.
  A check blaze proves holds where it is, and a call of `unwrap`, `expect` or a slice's or
  `Vec`'s index it proves cannot panic there, is not one either (the same proof
  [`unreachable`](#unreachable) uses: `v[i]` under `if i < v.len()` or in
  `for i in 0..v.len()`, `o.unwrap()` under `if o.is_some()` or `if let Some(_) = o`, `a / b`
  under `if b != 0`).
- `noalloc`: call the global allocator (`Vec::new`, `Box::new`, `to_owned`, `malloc`). An
  allocator the code names (`Vec::new_in(frame)`) is allowed.
- `nonblocking`: block the thread: std's and `parking_lot`'s locks, condvars and channels,
  `thread::sleep`, `JoinHandle::join`, and the syscalls that wait (`read`, `write`, `futex`,
  `poll`, `accept`, `connect`).

Each also refuses what it can't see: a function pointer the function takes, a call through a
`dyn` it takes whose method has no `#[blaze::expect(..)]` leaving the word out (taking the `dyn`,
storing it or handing it on is fine; the finding names the method called), and (`noalloc`,
`nonblocking`) a foreign function or `asm!` it knows nothing of. The
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

These three are the old spellings of `#[blaze::expect(..)]`, next.

## `loop_turn_effects`, `sync_on_answer_path`

**What they catch.** A function marked with its role (blaze extensions, opt-in; they change
diagnostics, never code) that may, through any call:

- `loop_turn_effects`, on `#[blaze::bounded_turn]` (a loop thread's turn): block the thread, do
  IO, sync a file to the disk or take a lock;
- `sync_on_answer_path`, on `#[blaze::answers_request]` (the path that answers a request):
  sync a file to the disk (`fsync`, `File::sync_all`, `blaze_sys`'s `fsync`).

What the walk can't see (a call it can't resolve, a foreign function no table knows) is refused
by name. The finding says the shortest call chain, each step with its file and line. The
design is [v2/plan/design/effect-summary.md](v2/plan/design/effect-summary.md).

**Why it matters.** A turn that waits stalls every client of the loop; an answer that waits
for the disk makes every request as slow as the slowest sync.

**How to fix it.** Hand the work to a worker or the driver, or mark a callee checked by hand
`#[blaze::bounded_turn(trusted = "why")]` (an accessor of a lock only the loop takes).

## `lock_order`, `lock_across_wait`

In the group `blaze::internal`, allowed unless a workspace turns them on: two locks the class
can't tell apart share one, so a cycle between them may be false.

**What they catch.** Over every function, through any call, with a lock classed by where it is
declared: `Type.field` (through an `Arc` or a `Box`, `Type.field[]` for an element), a static's
path, the name a `blaze_core::lock` lock is made with in the same body (`"store.kept"`), the
accessor's path for a guard a function returns of a lock it knows only by type, else
`type` and the lock's type (a lock handed in as a parameter, or read out of a collection).

- `lock_order`: the crate's functions take two locks in both orders, A then B and B then A. The
  finding prints both chains.
- `lock_across_wait`: a guard held across a call that blocks the thread (a condvar it is not
  handed, a channel, a sleep, a join, a blocking syscall). A fiber's park is
  `held_across_wait`'s, IO and allocation `lock_held_across_slow`'s.

Each reports only what it can show. Where the check can't see everything (a callee whose body
does not lower, more locks than a summary keeps) it says nothing, except at a function that
asked for its locks to be proved: `#[blaze::lock_checked]` on it, `#![blaze::lock_checked]` on
a module around it, or a role (`#[blaze::bounded_turn]`, `#[blaze::answers_request]`). There
it refuses: the check cannot show the absence of a cycle through it.

**How to fix it.** Take the locks in one order everywhere; drop the guard before the call.

**Allowing it.** `#[expect(blaze::lock_order, reason = "..")]` on the function.

## `loop_match_candidate`

In `blaze::pedantic_perf`, allowed by default.

**What it catches.** A hand-written state machine: a `loop` whose whole body is one of

- `state = match state { .. };`, each arm's value the next state, or the same `match` alone in a
  labeled block (`state = 'next: { match state { .. } };`, the form below with the attributes
  left off), where a `break 'next VALUE` ending an arm is a transition too;
- `match state { A => { ..; state = B; } .. }`, each arm ending in an assignment to `state`.

`state` is a local (not a field, not a closure's capture) of a type rustc's `loop_match` takes:
an integer, a float, `bool`, `char`, or an enum whose variants have no fields. No arm has a
guard, nothing else writes `state`, and at least one transition goes to a constant: a literal, a
fieldless variant or a module's `const`. The finding counts the transitions and how many are
constant. A loop that has `#[loop_match]` already is left alone, as is any shape it can't tell.
A state whose type needs drop (an enum with a `Drop` impl) is left alone too: rustc does not
drop the old state inside a `#[loop_match]` while blaze does, so the rewrite would change what
the loop does under one of the two compilers.

**Why it matters.** Each step goes back to the top of the loop and through the `match` again, an
indirect jump or a chain of compares the branch predictor has to guess. With `#[loop_match]`,
rustc compiles a constant transition into a direct jump to its arm, the way a C `goto` or a
computed-goto interpreter does.

**How to fix it.** On nightly, with `#![feature(loop_match)]` on the crate:

```rust
#[loop_match]
'machine: loop {
    state = 'blk: {
        match state {
            State::A => {
                work();
                #[const_continue]
                break 'blk State::B;
            }
            State::B => break 'machine,
        }
    };
}
```

A `break` out of the loop needs the loop's label, since Rust refuses an unlabeled `break` inside
the labeled block (E0695).

**Why it is off.** `loop_match` is an incomplete nightly feature: rustc warns
`incomplete_features`, and stable Rust rejects the attributes. The lint also can't see whether
the loop is hot.

**Allowing it.** `#[expect(blaze::loop_match_candidate, reason = "..")]` on the function.

## `undeclared_effect`, `unfulfilled_effect`, `negative_effect`

`#[blaze::expect(panic, alloc, block, diverge)]` (a blaze extension, opt-in; it changes
diagnostics, never code) lists what a function, and everything it calls, is expected to do:
panic or abort, call the global allocator, block the thread, not return. A word the list leaves
out is an effect the function does not have; a word a function's own list has is an effect it
has, as Rust's `#[expect(lint)]` is fulfilled or reported. The design, the rules and the tables
are in [v2/plan/design/effects.md](v2/plan/design/effects.md).

**What they catch.**

- `undeclared_effect` (`deny`): a function with an effect its allowance leaves out. The
  allowance is the function's own list; else the list of the trait's method it implements (or
  of the trait); else the `#![blaze::expect(..)]` of the nearest module around it; else
  nothing at all in a crate with `#![blaze::effects]`. The first written holds alone: a
  function's own list replaces a module's default, it is not added to it. A function none of
  these reach is not looked at, so a crate that declares nothing is unaffected. The finding
  says each effect with its witness: the shortest call chain to what has it, each step with its
  file and line, or for `diverge` the line of a loop not known to end, or the chain of a
  recursion and which function is called again. Also: an impl's method whose list has a word
  its trait's method does not allow (an impl may declare fewer effects, never more), a function
  with both a list and an old spelling, a list that is not words separated by commas, and the
  deleted spelling `#[blaze::may(..)]` (or `#![blaze::may(..)]`), wherever it is written, whose
  fix respells it `blaze::expect`.
- `unfulfilled_effect` (`deny`): a word of a function's own list for an effect the function is
  proved not to have, on every function with a body that writes its list, private and public
  alike, an impl's method that writes its own included. The finding names the function, the
  word and why it is known not to hold ("no call or operation in it allocates from the global
  heap, through every call it makes"); the fix takes the word off. `panic` is found only where
  the crate is checked with overflow checks, so the shorter list holds in dev and in release.
  Where exactness has nothing to prove, a list is a bound and never reported: a trait method's
  list bounds its impls (a default body among them); a list a function inherits from its trait
  or a module's default and did not write cannot be exact for every function under it; a
  `trusted` list is taken at its word; and a word over code the check can't see (a `dyn` call
  with no list, a body that does not lower) is neither proved nor refuted.
- `negative_effect` (`warn`): `#[blaze::nopanic]`, `#[blaze::noalloc]` or `#[blaze::nonblocking]`
  on a function under a module's default or `#![blaze::effects]`. The old spellings still mean
  what they meant, everywhere; where effects are declared, the function says what it is
  expected to do.

`unneeded_effect`, the advice this was before lists were exact (private functions only), is
deleted: naming it is `renamed_and_removed_lints`.

`diverge` in short: a counted `while i < n { ..; i += 1; }` is known to end (with the crate's
overflow checks, any positive step and `<=` too); every other loop (`for`, `while let`,
`loop`), recursion, and a call through a function pointer or a `dyn` whose trait method has no
list need the word, until `decreases` clauses exist. What the check can't see (an unknown
foreign function, a function pointer or unlisted `dyn` parameter) may do anything, so it needs
every word.

**Why it matters.** A signature that lists what can happen is complete and true: a reader, and
a caller on a pool thread, in a signal handler or under a deadline, knows what the call can do
without reading its body, the list keeps holding as the code changes, in every crate it
reaches, and a word in it is never stale.

**How to fix it.** `blaze check --fix` writes each list: the missing words added
(`undeclared_effect`; under `#![blaze::effects]` that is a `#[blaze::expect(..)]` with exactly
the inferred effects on every function that needs one), the unfulfilled words taken off
(`unfulfilled_effect`; a callee that stops having an effect makes each caller's word for it
unfulfilled, and this fix carries the change up the callers), the old spellings rewritten to the
list they stand for (`negative_effect`: `nopanic` and `nonblocking` on a function that
allocates and may not return become `#[blaze::expect(alloc, diverge)]`), and `blaze::may`
respelled. Or handle the effect so the function no longer has it (`get` or `checked_*`, the
task's frame, the scheduler's wait, a counted loop), or mark a callee proven by hand
`#[blaze::expect(.., trusted = "why")]`: its list is then taken at its word, and the reason is
kept in the proof.

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
through is refused too: a function pointer (take a type parameter), a `dyn` the function calls
an unlisted method through, a raw pointer, or
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

## `capabilities`

**What it catches.** A function of a crate whose root says `#![blaze::capabilities]` (a blaze
extension, opt-in; it changes diagnostics, never code) that, through any call, reaches one of
three effects past the crates its workspace trusts:

- `ambient`: std's file, environment, network, clock, randomness, process and thread entry
  points, or a system call or other foreign call beneath them;
- `unsafe`: an `unsafe` block written in the reached code, or an `unsafe fn`'s body (the block
  `format_args!` makes, and a macro defined `#[allow_internal_unsafe]`, are not);
- `global_alloc`: a call of the global allocator. An allocator the code is handed
  (`Vec::new_in(arena)`) is not one.

The finding names the function, the effect, the call or block that does it and the call path,
each step with its file and line. It is `deny` by default: the crate asked for it. A crate
without the attribute is not checked. What counts and why is in
[v2/plan/design/std.md](v2/plan/design/std.md).

**The trusted crates.** A workspace lists the crates whose code it has checked by hand:

```toml
[workspace.metadata.blaze.trusted]
libc = { owner = "io", reason = "the C library's calls, behind bstd's Io" }
```

Each entry needs an `owner` and a `reason`; a toolchain crate can't be listed. What a trusted
crate does itself, and what it reaches in the crates it depends on, does not count for any of
the three effects; a closure or impl of another crate it calls still does. core and alloc are
trusted for `ambient` and `unsafe` by default, but `alloc`'s allocation of the global heap
(`Vec::new`) still counts for `global_alloc`; the rest of the toolchain is trusted for `unsafe`.

**Why it matters.** A capability-based crate reaches the outside world only through what it is
handed, so what a caller gives it is all it can do; the promise holds only when nothing it
reaches, in any crate, takes the world another way.

**How to fix it.** Take the IO, the allocator or the safe interface as an argument, move the
code into a trusted crate (with an owner and a reason), or take the attribute off.

**Allowing it.** `#[expect(blaze::capabilities, reason = "..")]` on the function.

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
overflow. A `drop` that calls `std::thread::panicking()` anywhere is not checked. A site blaze
proves cannot fail where it is (an index under `if i < v.len()`, an `unwrap` under
`if o.is_some()`, a division under `if b != 0`: see [`unreachable`](#unreachable)) is not
counted, here or in `unwind_across_ffi`.

**Allowing it.** `#[expect(blaze::panic_in_drop, reason = "..")]` on the `drop` function.

## `unreachable`

**What it catches.** A `blaze::unreachable!()` (a blaze extension, opt-in:
[the design](v2/plan/design/proved-unreachable.md)) that blaze cannot prove no path reaches.
An error that no level waives: it fires only where the code wrote the macro. The finding is at
the site and names the function and what could not be shown:

```text
error: `get`: can't prove this `blaze::unreachable!()` is never reached: can't show `i < v.len()` here: no check before this branch rules the path out
```

**What blaze proves.** One pass over the function's own MIR, before any optimization, so the
answer is the same in every profile. It follows the checks that dominate the site: a `match`,
`if let`, `is_some`/`is_none`/`is_ok`/`is_err` (the variant is known after), a comparison with
`len()` or `is_empty()` of a slice, `str`, `Vec` or `String`, `for i in 0..v.len()`, a
comparison with zero or a `NonZero`'s `get()`, comparisons of integers with constants, and
code after an expression that never returns. It forgets what it knew of a value when the value
is reassigned, and of what a `&mut` leads to at every call and write through a pointer. It
follows linear arithmetic where the sum is shown not to wrap (`v[v.len() - 1]` under
`!v.is_empty()`, `v[i + 1]` under `i + 1 < v.len()`, `lo + (hi - lo) / 2` under
`lo < hi && hi <= v.len()`, and a binary search written as a loop), within a small budget of
facts added up. It does not follow products, a relation between two values that both change
round a loop, or what a function of your own returns.

**How to fix it.** Add the check that rules the path out so that it dominates the site. Where
blaze cannot prove it, return a `Result`, or write core's `unreachable!()`, the promise blaze
does not check, which counts as a panic site (`nopanic`, `panic_in_drop`).

**At run time** the macro is a panic, like `unreachable!()`, never undefined behaviour:
defence in depth for code whose lints were not run.

**Allowing it.** It can't be. `#[allow(blaze::unreachable)]`, `#[expect(..)]` and `#[warn(..)]`
are each an error at the attribute; a flag or a line of a manifest's lint table
(`[lints]`, `[workspace.metadata.blaze.lints]`, its ratchet, a baseline) that sets it below
`deny` refuses the check, naming the lint; `--cap-lints`, `warnings` and a group that covers
it do not reach it.

## `todo`

**What it catches.** A `todo!()` in code that is not a test (`#[test]`, `#[cfg(test)]`): a
hole, as in Agda and Haskell. The finding names the function, the type the expression must
have there and the bindings in scope that have that type:

```text
warning: `todo!()` in `open`: needs `Result<Dir, FsError>`; in scope: `cached`
```

**Levels.** `warn` in every local build, dev and release alike. The merge gate and a build
for a release are to deny it with `blaze check --deny blaze::todo`; today the gate's
`lint-baselines` job does, and the steps that run the released blaze wait for a release that
has the option (docs/v2/plan/design/proved-unreachable.md). The prover
behind [`unreachable`](#unreachable) never discharges a `todo!()`: it is unfinished, not dead.

**Allowing it.** `#[expect(blaze::todo, reason = "..")]` on the function.

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

## `proc_macro`

The group `blaze::proc_macro` of three lints, `proc_macro_derive`, `proc_macro_attribute` and
`proc_macro_fn_like`, each in `blaze::edition`; their defaults are in
[the table above](#the-general-lints).

**What it catches.** A procedural macro a crate of the workspace defines: a `#[proc_macro]`,
`#[proc_macro_derive(..)]` or `#[proc_macro_attribute]` function of a crate with
`proc-macro = true`. One finding per macro, at the function's name, naming the macro as it is
called (a derive by the name it is derived by) and its kind:

```text
warning: the function-like macro `sql` is a procedural macro, which blaze cannot see through
  = note: a procedural macro is opaque code that runs at build time and rewrites tokens: what it generates is not checked until it is expanded, cannot be analysed or cached per item, and reruns whenever the tokens of its input move
  = help: a macro that builds data at compile time is a `const fn` or a `const { .. }` block, which blaze checks once, analyses and caches as ordinary code (`core::mem::type_info` answers there, docs/v2/plan/design/comptime.md); a macro that parses a foreign syntax (a DSL in a string or in tokens) is not replaced by reflection and may stay: say so with `#[expect(blaze::proc_macro_fn_like, reason = "parses ..")]`
```

The first line is the same for "the derive macro `Describe`" and "the attribute macro
`route`"; the help is the kind's:

| kind | lint | help today |
|---|---|---|
| derive | `proc_macro_derive` | "nothing to change yet: the replacement is a generic impl over the type's fields by compile-time reflection (`for const f in fields_of::<T>()`), phase 2 of docs/v2/plan/design/comptime.md, which is behind the opt-in `#![feature(blaze_comptime)]` and not complete in this blaze; this lint is `allow` by default until it is" |
| attribute | `proc_macro_attribute` | "nothing to change yet: the replacement is reflection over the item (`fields_of::<T>()`, `fn_info(&f)`) and a generic function that does what the generated impls or registrations do, phase 2 of docs/v2/plan/design/comptime.md, which is behind the opt-in `#![feature(blaze_comptime)]` and not complete in this blaze; this lint is `allow` by default until it is" |
| function-like | `proc_macro_fn_like` | as above |

Once phase 2 is built the first two say what to write: "write the derive as a generic impl over
the type's fields by compile-time reflection: `for const f in fields_of::<T>()` reading
`value.[f]`, bounded by `for<F: Field<T>> F::Type: Trait`, under `#![feature(blaze_comptime)]`"
and "write what the attribute generates as a generic function over the item by compile-time
reflection: its fields with `for const f in fields_of::<T>()`, a function's facts with
`fn_info(&f)`".

**Why it matters.** blaze checks, caches and reruns code per item. A procedural macro is a
program that takes tokens and returns tokens: nothing about what it generates is known until it
has run, an error in its output is reported in code nobody wrote, and it runs again whenever
the tokens of its input move, a comment or a reordering included. Code written with
[compile-time reflection](v2/plan/design/comptime.md) is ordinary typed code, checked once.

**How to fix it.** A function-like macro that builds data at compile time: a `const fn` or a
`const` block, today. A derive or an attribute macro: nothing yet; phase 1 of reflection
(`core::mem::type_info`) is in blaze's const evaluator, and phase 2, which replaces them, is
behind the opt-in `#![feature(blaze_comptime)]` and not complete. A macro that parses a
foreign syntax (a query language, a template, a grammar in a string or in tokens) is not
replaced by reflection and may stay.

**Not found.** A use of a macro another crate defines (`#[derive(Serialize)]`): a workspace
cannot rewrite serde, so only definitions are findings, and there is no lint that lists uses.
A `macro_rules!` macro, which blaze expands and sees through. A dependency's proc-macro crate,
which is not linted.

**No fix.** The rewrite is the author's.

**Allowing it.** `#[expect(blaze::proc_macro_fn_like, reason = "parses ..")]` (or the kind's
lint) on the macro's function, or `#![allow(blaze::proc_macro)]` for the crate.

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

## `program_lifetime`, `could_be_program_lifetime`, `could_be_ref32`

In `blaze::edition`. `program_lifetime` is `deny` where it fires (only on a type declared
`#[blaze::program_lifetime]`); the other two are allowed by default. Design:
[facts](v2/plan/design/facts.md).

**What they catch.**

- `program_lifetime`: a type declared `#[blaze::program_lifetime]` (its instances live until
  the program exits) that cannot be: it has a parameter or a `Drop`; a field holds a value with
  a `Drop`, borrows for less than `'static`, or is a type the check can't see into (the finding
  names the field path and the rule); it is `pub` and exported in a crate without
  `#![blaze::program_lifetime]` at its root; or a record literal of it is built anywhere but as
  the argument of `blaze_mem::ProgramArena::alloc`, of `Box::leak(Box::new(..))`, or in a
  `static`'s initializer. Tuple-struct constructors are not followed yet.
- `could_be_program_lifetime`: a private struct with named fields, no `Drop` and nothing in it
  that drops or borrows, whose every record literal in the crate is leaked or is in the
  initializer of a `static` that holds no cell, lock or atomic. The finding says where the first
  is kept, how many `Arc`/`Rc` clones of it the crate makes, and the gain. Silent for anything it
  cannot follow: a value bound to a local, returned, passed to another call or put in a table at
  run time; tuple structs, enums and unions; public types unless the crate opts in. No fix.
- `could_be_ref32`: a field of a struct stored many times (the element of a collection in the
  crate, or itself program-lifetime) that holds a `#[blaze::program_lifetime]` type as
  `&'static T`, `Arc<T>`, `Rc<T>` or `Box<T>`, directly or as the element of the field's
  collection or `Option`. Never a local, a parameter or a return.

**Why.** A `blaze_mem::Ref32<T>` is 4 bytes, `Copy`, and derefs to `&'static T`;
`Option<Ref32<T>>` is 4 bytes too. A `&'static T` or a `Box<T>` is 8, and an `Arc<T>` is 8 with
two counts beside the value and a write for every clone.

**Fix.** `could_be_ref32` is machine-applicable for a private, plain `&'static T` field that
nothing assigns after the struct is built, no pattern takes apart, and every record literal
writes as `r.get()` of a `Ref32<T>`: the field becomes `blaze_mem::Ref32<T>`, each literal
writes `r`, and each read `s.f` becomes `s.f.get()`. The other two have none.

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

## `std_use`

In `blaze::edition`; its default is in [the table above](#the-general-lints).

**What it catches.** A use of an item of Rust's standard library that blaze's own standard
library ([bstd](v2/plan/design/std.md)) replaces or forbids, naming what is written instead.
What is replaced is one table (`TABLE` in `crates/blaze-lints/src/blaze/std_use.rs`), a row per
group of items; bstd's API is still moving, so a change there is a change of a row. Each row
has a kind:

- **ambient authority**: the file system, the environment, processes, the network, the clock,
  the standard streams, threads, thread-locals, `static mut` and randomness, reached from
  nowhere. A capability, or a value someone owns and passes, replaces each.
- **hidden cost**: a call that decides something costly with no argument for it: durability
  (`fs::write`, `fs::rename`, `File::sync_all`), a buffer lost on drop (`BufWriter`), a wait
  with no deadline (`thread::park`), a lock that poisons.
- **global-heap allocation**: `Vec::new`, `Box::new`, `String::from`, `format!`, `vec!` and
  the like. `blaze::hot_alloc` and `blaze::implicit_allocator` (of `blaze::internal`) already
  find these (and `collect()`, by its target type), so this lint leaves such a row to them
  wherever one of them is on: one use has one finding. The same holds for the hash rows and
  `blaze::default_hasher`.
- **renamed**: a type bstd re-exports under its one name (`Arc` is `bstd::sync::Shared`). The
  two names are one definition, so a path counts only where the type's segment is written by
  std's name; `bstd::sync::Shared::new(..)` is not a finding, or the port could never end.

The rows, in the order a function is matched against them (a named function before its
module's catch-all); "fix" says whether a finding carries a machine-applicable fix, "left to"
which lints report the row in this one's place where they are on:

| std's item | kind | replaced by | fix | left to |
|---|---|---|---|---|
| `std::fs::{write}` | hidden cost | `dir.write(cx, path, bytes, Durable::..)` or `dir.publish(path, content, durable)` on a `Dir`: durability is an argument with no default | no |  |
| `std::fs::{rename}` | hidden cost | `dir.rename(cx, from, to, Durable::..)` or `dir.publish(path, content, durable)` on a `Dir` | no |  |
| `std::fs::{copy}` | hidden cost | `flushed.clone_to(..)`, which exists on a `Flushed` file alone, or `copy_to(..)`, a plain write of the bytes | no |  |
| `std::fs::{File}::{sync_all, sync_data}` | hidden cost | `written.flush(cx)`, which gives a `Flushed`, or `Durable::Yes` on the write | no |  |
| `std::io::{BufWriter, LineWriter}` | hidden cost | build the bytes in a buffer you own (`mem::Bytes`), write them with one call, and end the file with `file.close(cx)?` | no |  |
| `std::thread::{park, park_timeout}` | hidden cost | the one wait of `Cx`, which ends in what it waited for or in a `Stopped` | no |  |
| `std::sync::{Mutex, RwLock, MutexGuard, RwLockReadGuard, RwLockWriteGuard}` | hidden cost | `Lock` and `RwLock` of `bstd::sync`, each made with its name, whose `lock` returns the guard and not a `Result` | no |  |
| `std::fs`'s functions | ambient authority | take a `Dir` (from `Fs`: `io.fs().open_dir(cx, path)`) and call its method (`dir.read_all(cx, path, &mut bytes)`, `dir.stat(cx, path)`, `dir.remove(cx, path)`) | no |  |
| `std::fs::{File, OpenOptions, DirBuilder, ReadDir, DirEntry}` | ambient authority | take a `Dir` and open beneath it (`dir.open(cx, path)`, `dir.create(cx, path)`, `dir.list(cx, frame)`), which gives a `File` of that directory's rights | no |  |
| `std::env::{var, var_os, vars, vars_os, set_var, remove_var}` | ambient authority | take an `Env` and call `env.get(&var!("NAME"))`; hand on less with `env.only(..)` | no |  |
| `std::env::{args, args_os}` | ambient authority | take `Args` (`io.args()`) | no |  |
| `std::env::{current_dir, set_current_dir, current_exe, temp_dir, home_dir}` | ambient authority | take the `Dir` the path stood for; the home directory is `User`'s | no |  |
| `std::process::{exit, abort, id}` | ambient authority | take a `Host` (`host.exit(status)`, `host.pid()`), or return from `main` | no |  |
| `std::process::{Command, Child}` | ambient authority | take a `Runner` of a sandbox and call `run.run(cx, &Command::new(prog!("..")))`: what the program can touch is the sandbox's spec | no |  |
| `std::net::{TcpStream, TcpListener, UdpSocket}` | ambient authority | take a `Net` (`net.connect(cx, &addr)`, `net.listen(cx, &addr)`), or the `Http` made over it | no |  |
| `std::time::{Instant, SystemTime}::{now, elapsed}` | ambient authority | take a `Clock` and call `clock.now()`; a simulated run then has a simulated time | no |  |
| `std::io::{stdin, stdout, stderr}` | ambient authority | take `Out` (`io.out()`) | no |  |
| `std::println!`, `std::eprintln!` | ambient authority | take `Out` and write to it; what a program says to a person or an agent is a `Report` | no |  |
| `std::thread::{sleep, sleep_until}` | ambient authority | take a `Clock` and call `clock.sleep(cx, wait)` | no |  |
| `std::thread::{available_parallelism}` | ambient authority | `host.cores()` of a `Host` | no |  |
| `std::thread::{LocalKey}` | ambient authority | `Cx` for the current span, deadline or runtime, `cx.frame()` for scratch, a local the task owns, or a `PerWorker<T>` someone passes | no |  |
| `std::thread`'s functions | ambient authority | start a task through `Cx`: `let s = cx.scope();`, `s.spawn("name", ..)`, `s.join(cx)?` | no |  |
| `std::thread::{Builder, JoinHandle, Thread, Scope, ScopedJoinHandle}` | ambient authority | a `Scope` from `cx.scope()` and the `Task` its `spawn` gives, joined with `join(cx)` | no |  |
| `std::thread_local!` | ambient authority | `Cx` for the current span, deadline or runtime, `cx.frame()` for scratch, a local the task owns, or a `PerWorker<T>` someone passes | no |  |
| `static mut` | ambient authority | a value someone owns and passes down, or a context | no |  |
| `std::sync::mpsc::{Sender, SyncSender, Receiver}` | ambient authority | the one channel of `bstd::chan` | no |  |
| `std::sync::mpsc::{channel, sync_channel}` | ambient authority | the one channel of `bstd::chan` | no |  |
| `std::sync::{Condvar, Barrier}` | ambient authority | `Signal` of `bstd::sync`, or a scope's `join(cx)` | no |  |
| `std::sync::{Once, OnceState}` | ambient authority | `Once<T>` of `bstd::sync`, a value to own and pass, never in a `static` | no |  |
| `std::hash::{RandomState, DefaultHasher}` | ambient authority | `bstd::hash::FixedState`, the one fixed seed; under keys an attacker picks, `SipKey` from an `Rng` | no | `blaze::default_hasher` |
| `std::collections::{HashMap, HashSet}` | ambient authority | `bstd::map::HashMap` and `bstd::set::HashSet`, made with `new_in(alloc)` | no | `blaze::default_hasher`, `blaze::implicit_allocator` |
| `std::random`'s functions | ambient authority | take an `Rng` (`rng.next_u64()`, `rng.fill(..)`), which is seeded in a simulation | no |  |
| `std::sync::{Arc}` | renamed | `bstd::sync::Shared` | yes |  |
| `std::rc::{Rc}` | renamed | `bstd::sync::LocalShared` | yes |  |
| `std::cell::{RefCell, OnceCell, LazyCell, UnsafeCell}` | renamed | `bstd::sync::LocalLock`, `bstd::sync::LocalOnce`, `bstd::sync::LocalLazy`, `bstd::cell::RawCell` | yes |  |
| `std::sync::{OnceLock, LazyLock}` | renamed | `bstd::sync::Once`, `bstd::sync::Lazy` | yes |  |
| `std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize}` | renamed | `bstd::sync::Atomic::<bool>`, `bstd::sync::Atomic::<u8>`, `bstd::sync::Atomic::<u16>`, `bstd::sync::Atomic::<u32>`, `bstd::sync::Atomic::<u64>`, `bstd::sync::Atomic::<usize>`, `bstd::sync::Atomic::<i8>`, `bstd::sync::Atomic::<i16>`, `bstd::sync::Atomic::<i32>`, `bstd::sync::Atomic::<i64>`, `bstd::sync::Atomic::<isize>` | yes |  |
| `std::vec::{Vec}::{new, with_capacity, from, from_iter, default}` | global-heap allocation | `Vec::new_in(alloc)` or `Vec::with_capacity_in(n, alloc)`, in the allocator the code was handed (`cx.frame()`, an `Arena`) | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::vec!` | global-heap allocation | `Vec::with_capacity_in(n, alloc)` and its pushes, in the allocator the code was handed | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::boxed::{Box}::{new, pin, from, default}` | global-heap allocation | `Box::new_in(value, alloc)` | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::string::{String}::{new, with_capacity, from, from_utf8, from_utf8_lossy, from_iter, default}` | global-heap allocation | `String::new_in(alloc)` of `bstd::mem`, text in the allocator the code was handed | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::string::ToString::to_string` | global-heap allocation | `format_in!(alloc, "{value}")` of `bstd::mem` | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::format!` | global-heap allocation | `format_in!(alloc, ..)` of `bstd::mem` | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::collections::{BTreeMap, BTreeSet, VecDeque, BinaryHeap, LinkedList}::{new, with_capacity, from, from_iter, default}` | global-heap allocation | its `new_in(alloc)`, in the allocator the code was handed | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |
| `std::path::{PathBuf}::{new, with_capacity, from, default}` | global-heap allocation | `bstd::path::Path`, relative and beneath a `Dir`; an owned one is `mem::PathBuf<A>` in an allocator | no | `blaze::hot_alloc`, `blaze::implicit_allocator` |

"fix" is this lint's. Where `blaze::implicit_allocator` reports a global-heap row in its place,
its own fix writes the explicit `Global` form for `Vec`'s and `Box::new`'s rows, the empty
`vec![]` and the collections with an `_in` constructor, in a crate with `allocator_api`, and for
no `String`, `to_string`, `format!` or `PathBuf` row: [its table](#implicit_allocator) says which.

Never found, because bstd re-exports it unchanged and it reaches nothing: `core::option`,
`core::result`, `core::iter`, `core::slice`, `core::str`, `core::num`, `core::cmp`,
`core::ops`, `core::mem`, `core::convert`, `core::fmt` and `core::time::Duration`. A function
of a module with a row that only computes (`env::split_paths`, `Instant::duration_since`, a
`Metadata`'s accessors) is not found either.

A use is found by what it resolves to, never by its text: a path that names a function, called
or not, through a `use`, an alias, a glob or a re-export; a method call, by the function it
resolves to; a trait's method, by the type it is called on (`file.write_all(..)` is a use of
`std::fs::File`, `String::from(..)` of `String`); a macro, by what its expansion calls
(`println!` is `std::io::_print`) or, for `thread_local!`, by the crate that defines it. There
is one finding per distinct item per function, at its first use there, with the count of its
uses in that function; a `static mut` and a `thread_local!` are found where they are declared:

```text
warning: `std::fs::read_to_string` (ambient authority), used 3 times in `fn load`, takes the file system from nowhere: take a `Dir` (from `Fs`: `io.fs().open_dir(cx, path)`) and call its method (`dir.read_all(cx, path, &mut bytes)`, `dir.stat(cx, path)`, `dir.remove(cx, path)`)
  = note: with no capability, code is pure computation: what a function can reach outside its own values is handed to it, so its signature is the whole list
  = note: blaze's standard library (bstd) is planned and not in this blaze: this is where the code is going, and there is nothing to change yet
```

The second note goes once `BSTD_IN_TREE` is `true`.

**Why it matters.** With no capability, code is pure computation: a function that is handed
nothing cannot touch a file, a socket, the clock or the environment, cannot wait, and cannot
allocate from a heap it was not given. Then a signature is the whole list of what a function
can do, a program runs unchanged under a simulated world, and every byte has an owner. Rust's
`std` lets any function do all of it, and nothing in its signature says so.

**How to fix it.** Take what the finding names as a parameter, from the caller that has it,
up to `main`, which receives the `Io`; a sans-IO core takes nothing and is handed bytes. This
is a change of signatures, done a crate at a time, leaves first: `harness/v2/std_uses.py`
writes the order and each crate's findings.

**The fix.** Only the renamed rows have one: `blaze check --fix` writes bstd's path where
std's is written (`Arc::new(x)` becomes `bstd::sync::Shared::new(x)`, `AtomicU32::new(0)`
becomes `bstd::sync::Atomic::<u32>::new(0)`), keeping the type's arguments. It is offered only
in a crate with a dependency named `bstd`, so it never writes a path that does not resolve. It
renames expressions; a type written in a signature or a `use` is the same type under either
name and is moved by `blaze check --replace std::sync::Arc=bstd::sync::Shared`. No other row
has a fix: each changes what a function is handed, and by whom.

**Levels.** `allow` by default, in every workspace, whatever the switch says. A crate asks
for it with `#![blaze::capabilities]` at its root (a module can too), which sets it to `warn`
beneath, as `#![warn(blaze::std_use)]` there would. A workspace asks with
`std_use = "warn"` in `[workspace.metadata.blaze.lints-when-ready]`, which holds once
`BSTD_IN_TREE` (`crates/blaze-lint-defs/src/ready.rs`) is `true`. blaze's own workspace denies
it ahead of the switch, in its ratchet, each crate not yet ported held at `warn` by its line in
`crates/blaze-lints/baselines/blaze.std_use.txt` ([the ratchet](#workspace-levels-and-the-ratchet)).
`-W blaze::std_use` in `RUSTFLAGS` turns it on for one run, which is how the worklist is made
today.

**Not found.** A build script: that is [`build_script_ambient`](#build_script_ambient), the
same table and the same walk, its rows those of the file system, the environment, processes,
the network and the clock, reported once per script. Test code (`#[cfg(test)]`, `#[test]`),
which drives the crate from outside. A type only named in a signature or a field: its value
came from a path that is found. A renamed type's path inside a macro call's arguments, or
under a name a `use .. as` gave it.
`AtomicPtr`, whose bstd form needs the pointee written. A `static` with interior mutability
that is not `static mut`, and what a call reaches through other crates: those are the effects
`global_state` and `ambient` of [the plan](v2/plan/design/std.md), not built yet. This lint
names what is written; the effects will prove what is reached.

**Allowing it.** `#[expect(blaze::std_use, reason = "..")]` on the function, or
`#![allow(blaze::std_use)]` in a module that owns the IO for now.

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

## `unmarked_pure`, `unmarked_nopanic`, `unmarked_noalloc`, `unmarked_nonblocking`

In `blaze::edition`, allowed by default. Each can be turned on alone.

**What it catches.** A function, method or associated function with a body that the effects
walk proves pure, nopanic, noalloc or nonblocking and that lacks `#[blaze::pure]`,
`#[blaze::nopanic]`, `#[blaze::noalloc]` or `#[blaze::nonblocking]`. The proof is the answer
the attribute's own lint (`pure`, `nopanic`, `noalloc`, `nonblocking`) computes for a marked
function, so the attribute is suggested only where it then passes: nothing is trusted but what
that check trusts, and where the walk can't decide (a body that does not lower, a call it can't
resolve) nothing is found. By default only a function at the crate's border is found: one with
a visibility written (`pub`, `pub(crate)`, `pub(in ..)`), or a method of a trait, or of an impl
of one, whose trait has one. `unmarked-effects-private = true` in `clippy.toml` finds every
function.

One finding per lint that is on and proved, each at the function's name with its own fix, so a
level set on the function (`#[expect(blaze::unmarked_noalloc, reason = "..")]`, `#[allow(..)]`)
works per lint, and `--fix` writes only the attributes of the lints left on there.

- A generic function is proved as a marked one is checked: in its own where-clauses, with what
  only an instance decides (a type parameter's method, its drop) left to each instance.
- `unmarked_nopanic` finds only in a crate checked with overflow checks (a dev profile, or
  `overflow-checks = true`), where a proof covers the checks release drops, so the mark holds
  in dev and in release. Checked without them it finds nothing.
- No attribute implies another: each refuses a kind of effect the others allow.
- In a crate with `#![blaze::capabilities]` the global heap is out of reach, so
  `unmarked_noalloc` finds only a function whose signature could allocate: a parameter (`self`
  included, behind references too) that is or holds a type implementing
  `core::alloc::Allocator`, a type with an allocator parameter (`Vec<T, A>`), or bstd's `Cx`.
  A signature with none already says noalloc.
- A method of an impl is found whether or not its trait is marked: the check reads the
  function's own attribute first. A method covered by its trait's attribute is not found.

**Why it matters.** The blaze edition infers effects inside a crate and writes them at its
border. A mark the code already earns lets callers rely on it (a `pure` caller needs no
`trusted`, the kernel caches a pure step), and from then on its lint refuses a change that
breaks it, naming the call chain.

**How to fix it.** `blaze check --fix` writes each attribute on its own line at the item's
indentation, keeping the function's effect attributes in the order pure, nopanic, noalloc,
nonblocking and moving none it has: before the first one written that comes later in that
order, else after the last one written, else directly above the signature, under the doc
comments and the other attributes. Attributes that land at one place are written one per pass
of `--fix`, so a function gaining all four takes its four passes.

**Not found.** A function already marked in any form (the bare word, `trusted = ".."`, a
malformed attribute, or the attribute on its trait); a trait method with no body; a test, a
bench, the crate's `main` and test code; a function a macro wrote.

**Allowing it.** `#[expect(blaze::unmarked_pure, reason = "..")]` (or the lint the finding
names) on the function, where the effect is an accident of today's body and not a promise.

## `unused_guard`

Planned with `#![blaze::early_drop]` ([early-drop](v2/plan/design/early-drop.md), objective
E14), in the group `blaze::edition`; `blaze check` does not report it yet.

**What it will catch.** `let _ = expr;` where a value of `expr`'s type keeps its scope's drop
under early drop (a lock guard, a file, anything with a `Drop` impl that is not marked
`#[blaze::drop(early)]`). `_` binds nothing, so the value is dropped at once: the lock is
released on the same line.

**How to fix it.** `let _guard = expr;` keeps it to the end of the scope (the fix `blaze check
--fix` will apply); `drop(expr);` says the immediate drop is meant.

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

# The guard lints

Three lints of `blaze::internal` (allowed unless a workspace turns the group on, as blaze's own
does) each hold a rule whose absence let bad code be written (rule `guard-the-cause`). Each asks
for a comment that states a reason on the line (`// durable: <why>`, `// wakes: <who>`,
`// identity: <why>`), read on the finding's line or the line above; an empty reason keeps
nothing. None is linted in tests. `implicit_allocator`, the last below, is the exception: no
comment keeps it, as no comment stands in for an allocator.

## `durable_write`

**The rule.** A write waits for the disk only where a crash would lose something no build can
make again, and its line says what.

**What it catches.** A `blaze_core::Effect::Write`, `Effect::Rename` or `Effect::Sync` built; a
call of `File::sync_all` or `File::sync_data` (std's, tokio's), of blaze-core's
`write_atomically`, or of `fsync`, `fdatasync` or `fcntl_fullfsync` (libc's, rustix's); each
with no `// durable: <why>`. The variant and the functions are resolved, so a renamed import is
found; a pattern that names a synced effect builds nothing and is not.

**Why it matters.** A sync holds its asker until the disk has the bytes, seconds on a busy disk.
A build's answer was held 4 s behind the sync of files the next build would have written again,
because nothing asked why they were synced.

**How to fix it.** Data a build can make again, and whose reader checks it before use (an
output placed from the store, a file named by its content), takes `Effect::WriteUnsynced` and
`Effect::RenameUnsynced`. What must survive (a commit point, a journal, a lease, a user's file)
keeps its sync and says so: `// durable: the manifest is the one record of which files a flush
wrote`.

**Not found.** A sync asked of the kernel by a raw opcode (blaze-core's io_uring driver, which
carries out `Effect::Sync`), and `fcntl(fd, F_FULLFSYNC)` through libc's `fcntl`.

**Allowing it.** The comment, or `#[expect(blaze::durable_write, reason = "..")]`.

## `unbounded_wait`

**The rule.** A wait has a deadline, or its line names what always wakes it and why that
cannot end first (rules `deadline-waits` and `bounded-progress`).

**What it catches.** A call of `std::thread::park`; of `Condvar::wait` or `wait_while` (std's,
`parking_lot`'s) or `blaze_core::lock::Signal`'s `wait` or `wait_while`; or of a function of
`blaze_core::wait` (`Parker::park`, `until`, `exited`, `readable`, `pollable`) given `None` for
its deadline, written out; each with no `// wakes: <who>`. `blaze_core::wait` itself, where the
deadline is applied, is not linted.

**Why it matters.** A wait with no deadline hangs forever the day its waker ends another way.
Inside the build-script sandbox the workers' wait ring was refused, each worker returned from
its error path, and the main thread stayed in `thread::park()`: blaze hung with no error.
`.clippy.toml` already banned `std::thread::park`, and the site carried an `#[expect]` whose
reason said why it parks, not who wakes it.

**How to fix it.** Wait to a `Deadline` through `blaze_core::wait` and end in a typed error
naming what was awaited when it passes (`Waited::Deadline`). Where no deadline is right (an
idle worker, a thread that serves until told to stop), name the waker and why it cannot end
without waking: `// wakes: the pool's Drop, which unparks every thread it listed`.

**Not found.** A channel's `recv` and a thread's `join`: the other side's end wakes them (the
last sender dropped, the thread returned), so a waker that ends silently still ends the wait. A
deadline that is `None` only at run time. A raw kernel wait, which `.clippy.toml` bans outside
`crates/blaze-core/src/wait.rs` and its modules.

**Allowing it.** The comment, or `#[expect(blaze::unbounded_wait, reason = "..")]`.

## `combined_identity`

**The rule.** An identity of many things is a hash of all of them; one kept through edits is
`blaze_ids::keyed::Keyed`'s root (`crates/blaze-ids/src/keyed.rs`), and nothing else adds or
XORs hashes into an identity.

**What it catches.** `a ^ b`, `a ^= b`, `a.wrapping_add(b)` or `a.wrapping_sub(b)` on integers,
in a function whose parameters or return type hold a `blaze_ids::ContentId` (itself or two
types deep: `BuiltId(ContentId)`, `&[(Key, ContentId)]`), or with an operand that reads one;
each with no `// identity: <why>`. `blaze_ids::keyed`, the one audited module, is not linted.

**Why it matters.** A sum or an XOR of hashes updates as cheaply as a Merkle root and is not
collision resistant: entries that XOR to a chosen value are found by linear algebra, entries
that sum to one by a generalized birthday search. Two different sets then share an ID, and
state kept under it is used for the wrong set (rule `keys-cover-every-input`). A build's ID
was the wrapping sum of its functions' entries, and keyed kept state.

**How to fix it.** Keep the set in a `Keyed` (set and remove entries as they change, about
`log2(n)` hashes each) and take its root; or hash every entry with `blaze_ids::Hasher` where
nothing is kept between builds.

**Not found.** `+` and `+=`, which panic on overflow where it is checked; an identity that is
a bare integer or array with no `ContentId` in the function's signature or the operands.

**Allowing it.** An XOR or a sum that makes no identity (a shard index, a checksum of one
buffer, a hash function's own rounds) says so with `// identity: <why>`, or
`#[expect(blaze::combined_identity, reason = "..")]`.

## `implicit_allocator`

**The rule.** In this order (rule `bump-allocate`, 2026-11-13): first, don't allocate where a
borrow, a fixed buffer or the caller's buffer does; second, where an allocation is needed,
allocate by lifetime and name the allocator: a task's frame (`Vec::new_in(frame)` from
`blaze_mem::frame::current`), a request's `ArenaLocal`, an image value's `Kept`, or, for the
global heap, `Global` itself (`Vec::new_in(Global)`). Never implied, and no comment stands in for
it.

**What it catches.**

- A call that makes a default-allocator value none of its inputs holds: `Vec::new()`,
  `Vec::with_capacity(n)`, `vec![..]`, `Box::new(..)`, `String::from(..)`, `format!(..)`,
  `collect()`.
- A copy, even of an input that already holds the type: a resolved `Clone::clone`,
  `ToOwned::to_owned`, `ToString::to_string` or `<[T]>::to_vec` whose value holds one. A clone of
  an `Arc` or `Rc` is a count, not a copy, and is not found.
- A `let` whose binding holds what such a call made.
- A written type of std's that takes no allocator and owns heap memory: `String`, `PathBuf`,
  `OsString`, `CString`, std's `HashMap` and `HashSet`; a `Cow` of `str`, `[T]`, `Path`,
  `OsStr` or `CStr`, whose `Owned` is one of them.
- A type of a dependency that takes no allocator but holds one of these, found by walking its
  fields transitively (cached per type, a cycle cut): `BufReader`, `BufWriter`, `Command`,
  channels, `indexmap`, `smallvec`, `bytes`, `serde_json::Value`, `regex::Regex`. The message
  names the field: "`X` takes no allocator but holds a `Vec` (its field `buf`), so it allocates
  from the global heap". The workspace's own crates, whose allocations the lint finds where they
  are made, and `papaya` are not walked.

A type written with an allocator parameter left to its default, `Vec<T>` or `Box<T>`, is not a
finding where it is written: `Vec<T>` is `Vec<T, Global>`, so the allocation is found where the
value is made, and a signature need not change. A call given `Global` (`Vec::new_in(Global)`) is
not a finding. The allocator parameter is found by what it is (bounded by `Allocator` or
defaulting to `Global`), not by its name. It is `deny` in blaze's workspace, with
`blaze::hot_alloc`.

**Why it matters.** A value on the global heap belongs to no lifetime: it is freed object by
object, whenever its last owner drops it, and no task's release frees it. A value in its task's
frame goes, all at once, when the task does. An allocation not made costs nothing at all; one
whose allocator is named can be moved to a frame later by changing one argument, and is counted
until it is.

**How to fix it.** Each finding's help leads with the form that does not allocate, where one
exists:

| found | first, no allocation | then, named |
|---|---|---|
| `to_owned`, `clone`, `to_vec` | a borrow (`&str`, `&[T]`, `&T`) | `slice.to_vec_in(alloc)`, `blaze_mem::String::from_str_in(s, alloc)` |
| `format!`, `to_string` | `blaze_mem::format_into!(&mut buf, ..)` into the caller's buffer, or `write!` | `blaze_mem::format_in!(alloc, ..)` |
| `vec![a, b, ..]` | an array `[a, b, ..]`, or the caller's slice | `Vec::with_capacity_in(n, alloc)` and its pushes |
| `collect()` | pass the iterator on | `Vec::with_capacity_in(n, alloc)` and `extend` |
| `Vec::new()`, `Box::new(..)`, `String::from(..)` | a fixed buffer or the caller's | `Vec::new_in(alloc)`, `Box::new_in(x, alloc)`, `blaze_mem::String::from_str_in(s, alloc)` |

`blaze_mem::String<A>`, `PathBuf<A>`, `OsString<A>` and `blaze_hash::HashMap<K, V, A>` name an
allocator where std's types do not.

**The fix.** `blaze check --fix` writes the explicit global form, which keeps the meaning exactly:
not allocating, or allocating in the task's frame, is the better change and is made by hand (the
help says so). It is given where the crate root enables `#![feature(allocator_api)]` (a fix
cannot add the gate from another file; 74 of 150 crates have it) and the type has the `_in`
constructor:

| std's row | the lint's fix |
|---|---|
| `Vec::new()`, `Vec::with_capacity(n)` | yes: `Vec::new_in(Global)`, `Vec::with_capacity_in(n, Global)` |
| `vec![]` | the empty one only: `Vec::new_in(Global)`; the tree has no `vec_in!`, so `vec![a, b]` has none |
| `Box::new(x)` | yes: `Box::new_in(x, Global)`; `Box::pin`, `from`, `default`: no |
| `VecDeque`, `BTreeMap`, `BTreeSet`, `BinaryHeap`, `LinkedList` `new`/`with_capacity` | yes, where the type has `new_in`/`with_capacity_in` |
| `String`, `to_string`, `format!` | no: the explicit form is `blaze_mem::String`, another type (below) |
| `PathBuf`, `OsString`, std's `HashMap`/`HashSet` | no: another type |
| `collect()`, `clone`, `to_owned`, `to_vec` | no |

`Global` is named as it resolves at the site: `Global` where it is in scope, the path to it
where something else is, and where nothing is, the fix adds `use std::alloc::Global;` (by name
resolution) before the first statement of the function the call is in (a fix of a body writes
in that body's own tree, so not at the module's items). Many findings in one function each want
that import; `--fix` applies one of them and the rest on its next pass (it runs up to four).

**What a type change costs.** `Vec<T>` and `Vec<T, Global>` are one type, and so are `Box<T>`,
`VecDeque<T>`, `BTreeMap<K, V>` and the rest with an allocator parameter: the fix changes a
constructor and nothing else, and no signature or caller moves. It is not one type for `String`,
`PathBuf`, `OsString` and std's `HashMap`/`HashSet`, which take no allocator: their explicit
form is `blaze_mem::String<Global>`, `blaze_mem::PathBuf<Global>` or
`blaze_hash::HashMap<K, V, Global>`, so the change reaches every signature the value passes
through, a whole port (rule `whole-ports`), and no fix makes it.

**Allowing it.** No comment: `// alloc: <why>` keeps nothing since 2026-11-13. An item that must
keep the global heap implied says why with `#[expect(blaze::implicit_allocator, reason = "..")]`.
`scripts/src/bin/allocs.rs`, a step of the gate's `scripts` job that needs no compiler, holds
each crate, outside tests, to its row of `crates/blaze-lints/baselines/alloc-sites.txt`, in three
columns:

- `comments`: the `// alloc:` lines left to convert. It only goes down, to 0; once no crate has
  a row, every such line anywhere is refused.
- `expects`: the `allow`/`expect`s of this lint. It only goes down.
- `global`: the lines whose code names `Global`, the number moving to frames brings down. It
  rises while `// alloc:` lines are converted to it, and only goes down once none is left.

A crate over a row fails, naming the new lines; a crate under one fails until the row is
lowered (`{"command": "write"}`); a crate with no row, every new crate, is held to 0.

## `untied_child`

**The rule.** A process a test starts ends with the test: its `std::process::Child` is owned by
`stdx::JodChild`, which kills and reaps it on drop, and a `blaze` it runs is told to let its
daemon exit once nothing holds it (`BLAZE_DAEMON_EXIT_UNHELD`,
crates/blaze-cargo/src/serve/unheld.rs `VAR`).

**What it catches.** In test code (a body under `#[test]` or `#[cfg(test)]`, and every body of
an integration test target): a call whose value is a `Child` or a `Result` of one, that is
`std::process::Command::spawn` by name resolution or a function of the tree that wraps it (one
whose return type is a `Child` or a `Result` of one), unless the value goes straight into a
`JodChild` (`JodChild(cmd.spawn()?)`, `cmd.spawn().map(JodChild)`, through `?`, `unwrap`,
`expect`, `map_err`, `context`) or is waited for there and then (`cmd.spawn()?.wait()`). A
wrapper's own body is not linted; its callers are. And a `Command::new(env!("CARGO_BIN_EXE_blaze"))`
in a body that sets no `BLAZE_DAEMON_EXIT_UNHELD`: the body is taken to set it when it calls
`Command::env` with that key or a key that is not a literal, calls `Command::envs`, hands a
`Command` to a function of the tree, or returns one. `status()` and `output()` wait for their
child, but not for a daemon the child started, so this half applies to them too.

**Why it matters.** A dropped `Child` is neither killed nor waited for, so a test that panics or
fails on an assert before its `kill` leaves its process running, and a daemon a test's `blaze`
starts outlives the test however it ended. 44 daemons ran at once and 8 were orphaned when
tests were killed at their time cap.

**The fix.** `stdx::JodChild::spawn(command)`, or `.spawn().map(stdx::JodChild)` on a spawn
written in a chain (the fix offered, `MaybeIncorrect`, where the crate depends on `stdx`: a use
of the child by value, such as `wait_with_output`, then goes through `into_inner`). A struct
that keeps the child keeps the `JodChild`, not a `Child` with a hand-written `Drop` that kills
it. On a command that runs `blaze`: `.env("BLAZE_DAEMON_EXIT_UNHELD", "on")`.

**Not found.** A path to blaze held in a variable or a field (`Command::new(&self.blaze)`):
what it names is known only at run time. A `Child` handed into a function or struct that wraps
it in a `JodChild` (its type holds one) counts as owned.

**Allowing it.** `#[expect(blaze::untied_child, reason = "..")]` naming what ends the process
instead (a crate below `stdx`, which cannot name `JodChild`, is one).

## `unbounded_test_wait`

**The rule.** Every wait in a test has a deadline and fails naming what it waited for (rule
`bounded-progress`). What a test waits on is the code under test, which is what may be broken.

**What it catches.** In test code (a `#[test]` function, anything under `#[cfg(test)]`, every
body of an integration test's crate): a call of `std::thread::park`; of `Condvar::wait` or
`wait_while` (std's, `parking_lot`'s) or `blaze_core::lock::Signal`'s `wait` or `wait_while`; of a
function of `blaze_core::wait` given `None` for its deadline, written out; of a channel's
`Receiver::recv` (std's, `crossbeam_channel`'s); of `std::process::Child::wait` or
`wait_with_output`, unless the same child was killed earlier in the same block. And a `loop` or
`while` loop around `Child::try_wait` that neither sleeps nor waits (one that does is
`open_wait_loop`'s) and compares no deadline or round count where it ends, by a `break`, a
`return` or a panic. Calls are resolved by name, so a renamed import counts.

**Why it matters.** A test sat ten hours in `thread::park`: the code under test never unparked
it, and nothing said what it waited for, so the run held the machine until someone looked. A
test that waits to a deadline fails in seconds and names the wait. This is `unbounded_wait`'s
twin, not that lint widened: outside a test a channel's `recv` and a thread's `join` are ended by
the other side's end, and `// wakes: <who>` can name what always wakes a park; in a test the
other side is the code under test, whose bug is the hang, so neither answer holds and there is
no `// wakes:` exemption. `unbounded_wait` leaves all test code to this lint.

**The fix.** `rx.recv_timeout(WAIT).expect("<what was awaited>")`; for a child,
`stdx::child::exit_within(&mut child, "<what>", WAIT)` or `stdx::child::output_within(child,
"<what>", WAIT)`, which kill a child still running at the deadline and panic naming it; a park
or condition wait to a deadline (`park_timeout`, `Condvar::wait_timeout`, `Signal::wait_for`) in
a loop that compares `elapsed()`; a `blaze_core::wait` call given `Some(deadline)`; a `try_wait`
loop that compares a deadline or a round count and panics naming what hung.

**Not found.** `JoinHandle::join`: a joined thread hangs only in a wait of its own, found where it
is written; every join would be about 450 findings, nearly all of threads that only compute.
`Barrier::wait` (`.clippy.toml` bans `std::sync::Barrier`; the tests that keep one use it as a
start gate). `Command::output` and `status`, which the tree's tests use for tools that end
(`git`, `cc`, `kill`). A loop around `try_recv`, as often a drain as a wait. A deadline that is
`None` only at run time, and a reap after a bounded wait already saw the child exit.

**Allowing it.** `#[expect(blaze::unbounded_test_wait, reason = "<what bounds it>")]`, for a wait
something the lint cannot see bounds (a reap after `wait::exited` saw the child exit).

## `reads_positions`

**The rule.** A query of the front end reads only position-free forms (the body, the
signatures, the item tree, the outline); positions are read only by functions marked
`#[blaze::renders_positions]`, whose output is about where code is written (rules
`semantics-in-front-end` and `keys-cover-every-input`).

**What it catches.** In `hir_def`, `hir_ty`, `hir_ty_mir`, `hir_ty_borrowck`, `hir_ty_unsafeck`,
`hir_ty_pure`, `hir_ty_codegen`, `blaze_items` and `blaze_item_hash`, a call, closures included,
from a function not marked `#[blaze::renders_positions]` of a reader: `EditionedFileId::parse`;
`HirFileId::{root, parse_or_expand, ast_id_map}`; `InFile`'s (an `AstId`'s, an `InFile<AstPtr>`'s)
`root`, `file_syntax`, `to_node`, `to_view`, `to_ptr`, `to_range`; any associated `source_map`,
`relative`, `unmoved_source_map`, `unmoved` or `with_source_map` of `hir_def` (`Body`'s,
`ExpressionStore`'s, each signature's, `VariantFields`', `GenericParams`'); a source map's
`*_syntax` methods and `self_param`; any `source_view` or `*_source_view`; `HasSource::source`
and `ast_ptr`; and any function marked `#[blaze::renders_positions]`. The allowance passes to a
callee only through a marked function: an unmarked helper that reads a position is found even if
only marked functions call it, and an unmarked function that calls a marked one is found.
Not found: the base readers' own bodies; `hir_def`'s lowering modules (`item_tree`, `nameres`,
`expr_store`, `signatures`, `src`), which build the position-free forms and their source maps from
the syntax and are held to their firewalls by their own equality checks; tests; a reader passed
as a value rather than called.

**Why it matters.** A query that reads a position depends on every edit above it in its file, a
comment included, so its result is never kept across one. Every borrow check read its body's
source map, so no borrow-check result was ever kept; `const_check` and the attribute queries
read the whole file's parse and reran on any edit of it.

**The fix.** Read the fact the position stood in for from the lowered form:
`ExpressionStore::is_synthesized` in place of `source_map.expr_syntax(e).is_err()`, an item's
syntax off the outline (`signatures::outline_item`) in place of `AstId::to_view`. Where the output
is about positions (a diagnostic's words, debug info, a link's span tables, an IDE lookup), mark
the function `#[blaze::renders_positions]` (`#![feature(register_tool)]` and
`#![register_tool(blaze)]` at the crate root), and record the span in the query so the wording
happens in a marked function at the edge.

**Allowing it.** `#[expect(blaze::reads_positions, reason = "<what must change>")]` on the
function.

## `absolute_position`

**The rule.** A position that reaches code, a cache key or a kept value is item-relative: the
item by key and an offset or index in it (`blaze_format::SpanRef`, `blaze_format::RelSpan`, an
anchor-relative `span::Span`). The absolute `file:line:column` is made only where it is printed,
from where the item starts now (rules `keys-cover-every-input` and `granular-caching`). The other
half of `reads_positions`: that lint finds a query that reads a position, this one a value that
holds one.

**What it catches.** In blaze's workspace, outside tests, macro output and the text layer
(`text_size`, `line_index`, `rowan`, `syntax`, `syntax_tree`, `parser`, `span`, `tt`, `mbe`, the
`ide` crates, `blaze_fmt`), by name resolution and types: a field of a struct, or of an enum's
variant, whose type implements `Hash`, serde's `Serialize` or `Deserialize`, or `blaze_format`'s
`Record` or `Persist` (derived or by hand), that is or holds (through references, tuples, arrays
and type arguments) a `core::panic::Location`, a `text_size::TextRange`, a `line_index::LineCol`
or `WideLineCol`, or `hir_expand`'s `FileRange` or `FilePosition`; and anywhere, a field that
holds a `core::panic::Location` by value. Not found: `TextSize` (a length or an offset into any
text as often as into a file), a bare `u32` line (no type tells it from a count), the position
types' own definitions, and a `&'static Location` in a value that is neither hashed nor kept,
which is the link's record by reference.

**Why it matters.** A line added above a function changes every absolute position below it, so
a key, record or identity holding one moves although the code did not change, and what it keys
is made again. In code blaze builds a `Location` is the record the link places for one span of
one item and patches in place after a shift; a copy of it in a value goes stale under a live
patch.

**The fix.** Hold the item by key and the position within it; render where it is shown. In the
process a site is `blaze_core::site::Site`, the link's record by reference, with no `Hash` and no
`Ord`. No fix is applied for you: each item-relative type needs the item's anchor, which the code
that fills the field supplies.

**Allowing it.** `#[expect(blaze::absolute_position, reason = "..")]` on the type, for a value
that is shown and never a key.

## `cbor_field_order`

**What it catches.** In a module marked `#![blaze::canonical_cbor]` (or under one: the marker on
a crate root covers the crate), a struct, or a struct variant of an enum, that derives
`Serialize` (serde's own, `serde::Serialize` or `serde_derive`'s, or `blaze_serde`'s, found by
name resolution) with its fields declared out of the order of their keys' encoding: shorter
key first, then bytewise. A field's key is its `#[serde(rename = "..")]` (the `serialize` side
of `rename(..)`), else its name after the container's case rule (a struct's `rename_all`, a
variant's own `rename_all`, else its enum's `rename_all_fields`). A `#[serde(skip)]` or
`skip_serializing` field is no key. The message names the item, the first field out of place
and the field it goes before, with both keys.

**Why it matters.** RFC 8949's core deterministic CBOR (`blaze_protocol::cbor`) writes a map's
keys in the bytewise order of their encodings. A type declared in that order is written as it
comes; one declared in another has its entries found again, sorted and copied at the end of the
map. blaze-api's module doc promised the order for every struct, and seven of its structs broke
it: only the codec test's "written out of order" assertion
(`crates/blaze-api/src/frames/identity.rs`) found them, by chance of what its generator built.
serde's derive is crates.io's, not the tree's, so the promise is checked here, at the definition.

**The fix.** Move each field's whole text (attributes, doc comments and the comments attached to
it) into the order; `blaze check --fix` does, a skipped field keeping its place. The fix is
offered as maybe-incorrect, with a note, when the type derives `PartialOrd`, `Ord` or `Hash`
(the derived comparison or hash follows the declaration order, so the reorder changes what it
computes) or when a comment between the fields belongs to neither (it stays where it is).

**Not found.** A `#[serde(flatten)]` field, `#[serde(transparent)]`, `#[serde(into = "..")]`, a
`#[serde(..)]` inside a `cfg_attr`, a case rule other than `lowercase`, `UPPERCASE`,
`PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case` and
`SCREAMING-KEBAB-CASE`, and a rename that is not a plain string literal: the keys can't be told
from the syntax. An internally tagged item's tag, which the derive writes first wherever its key
sorts. Test code, an item a macro wrote (`blaze_protocol::in_key_order!`), an item in a function
body.

**Allowing it.** `#[expect(blaze::cbor_field_order, reason = "..")]` on the item (or the
variant), naming what needs the declaration order (blaze-api's `Envelope` keeps `schema` first
for its JSON line and writes its CBOR by hand in key order).

## `gapped_literal`

**What it catches.** A string literal written in the file (`".."`, `b".."`, `c".."`, in code
or a macro call's input, tests included) with a run of eight or more spaces on one of its
source lines between two characters of its text. Neither neighbour may be whitespace, the
opening quote, an escape `\n`, `\r` or `\t`, or a format placeholder `{..}` (`{{` and `}}`
count as text). A raw literal is never a finding.

**Why it matters.** A `\` at the end of a literal's line skips the newline and the next line's
leading whitespace. When the `\` and the newline are lost, the lines join and the value keeps
the indentation, so the message prints with a gap. Two lanes' scripts broke continuations this
way, and the gaps shipped (`agents/annoyances/lost-line-continuations.json`).

**The fix.** Put the `\` and the newline back; the suggestion does it. It is machine-applicable
when the run, less one space, is a whole indentation, which is the shape a joined continuation
leaves. A table or a picture aligned on purpose becomes a raw literal where that keeps its value.

**Allowing it.** `#[expect(blaze::gapped_literal, reason = "<what is aligned>")]` on the item
holding the literal, for example a test of a diagnostic picture.

## `retry`

**What it catches.** Outside tests, either of these:
- A comparison with a count whose name has a `_`-separated word `retry`, `retries`, `retried`,
  `attempt`, `attempts`, `tries` or `failures`, against a constant bound. The bound is an
  integer literal other than `0` and `1`, a `const` or `static`, or another count named the
  same way.
- A `for` loop over a range that ends at such a constant, or whose pattern binds such a name.

**Why it matters.** Rule `no-fallback`: no retry, ever. A retry repeats the failure, hides its
first cause, and makes how long an operation takes depend on how often it fails. blaze-cas's
`FAILURES_MOST = 8` repeated failed IO eight times before it said anything.

**The fix.** Remove the loop. The first failure is the error: typed, and naming what failed and
where.

**False positives.** Few are expected. One is a count of reported failures compared with a
threshold for listing them, which is not a retry. `failures == 0` and `failures > 1` are not
findings.

**Allowing it.** `#[expect(blaze::retry, reason = "..")]` on the function, saying why it is not
a retry or what must change.

## `unwired`

**What it catches.** Outside tests, an `#[allow(..)]` or `#[expect(..)]` whose list names
`dead_code` or `unused`, outer or inner, written plainly or in a `cfg_attr` the build keeps. It
must be on an item, an associated item, a field or a variant. Other lints of the `unused` group
are not findings. Neither is the attribute on a `use`, on `extern crate`, on a statement, on an
expression or on a parameter.

**Why it matters.** Whole designs sat written and never wired in: `mod found`, and
`job/certificate.rs`. The attribute hid that nothing called them, so they rotted unseen.

**The fix.** Do one of these:
- Wire the item where it was meant to be used.
- Delete it.
- If it is used only on some targets, give it their `cfg`.

**Allowing it.** `#[expect(blaze::unwired, reason = "<what wires it, and when>")]` beside the
attribute. This is the one way to keep one.

## `tmp_path`

**What it catches.** Either of these:
- A string literal, raw or not, tests included, whose value starts with `/tmp`, `/private/tmp`
  or `/var/tmp`, followed by the literal's end or a `/`.
- A use of `std::env::temp_dir`, a call or the function named as a value, found by name
  resolution, outside the module `temp` of `blaze_core`.

**Why it matters.** Test homes made there by hand leaked, one per run. A reboot also wiped a
round's notes that were kept there.

**The fix.** `blaze_core::temp::TempDir` is the one helper. `TempDir::new(prefix)` makes a
directory of its own, and `TempDir::for_test(prefix)` does the same in a test. Either is removed
with everything it holds when it drops, so hold it for as long as the directory is used. A file
that must sit beside the system's temporary files takes its directory from
`blaze_core::temp::base()`. `stdx::tempfile::NamedTempDir` and `blaze_script::TempDir` were the
same helper again, and they go.

**Allowing it.** `#[expect(blaze::tmp_path, reason = "..")]` on the item, for a literal that names
the real `/tmp` as policy, such as a sandbox profile or a mount table.

## `narrowed_len`

**What it catches.** Outside tests, `T::try_from(x)` or `x.try_into()` to an integer type
narrower than `x`'s, unwrapped at once with `.expect(..)` or `.unwrap()`, where `x` names a
length. That means `x` holds a call of `len`, `count`, `size` or `size_of`, or a path or field
whose name has a `_`-separated word `len`, `length`, `size`, `offset`, `count` or `bytes`.
`usize` and `isize` count as 32 bits wide as a target and 64 as a source. An `as` cast is
`lossy_as`'s. A handled error (`map_err(..)?`, `unwrap_or(..)`) is not a finding.

**Why it matters.** A 4 GiB file panicked the daemon's main thread at "a record under 4 GiB". A
length is as large as its input.

**The fix.** Return a typed error that names the thing and its size, such as
`map_err(|_| Error::RecordTooLarge { what, len })?`.

**Allowing it.** `#[expect(blaze::narrowed_len, reason = "<the bound and where it is checked>")]`
on the function, where a check just before bounds the value.

## `process_static`

**What it catches.** Outside tests, a `static` or a key of a `thread_local!` whose resolved type
holds either of these:
- A growable collection (`Vec`, `VecDeque`, `BinaryHeap`, `LinkedList`, `String`, `PathBuf`,
  `OsString`, and the hash and B-tree maps and sets of std, hashbrown, blaze_hash and indexmap)
  beneath a lock or a cell. That means std's `Mutex`, `RwLock`, `RefCell`, `Cell` and
  `UnsafeCell`, `blaze_core::lock`'s `Lock` and `ReadLock`, and any `*Lock` or `*Mutex` other
  than `LazyLock` and `OnceLock`.
- A concurrent collection: papaya's and dashmap's maps and sets, or crossbeam's `SegQueue`.

The type is followed through once-cells, `Option`, `Arc`, `Rc`, `Box`, tuples, arrays and the
fields of the tree's own types, up to four types deep. These are not findings: atomics, plain
data, a collection that is set once and has no lock or cell inside (`OnceLock<Vec<T>>`), and an
`ArrayQueue`. `global_state` names every interior-mutable static for the edition; this lint is
the narrow deny of the ones that grow.

**Why it matters.** Codegen's `KEPT`, `WITNESSES` and `FOUND` grew for the daemon's whole life,
which is days. Every build after the first read what the earlier ones had left.

**The fix.** Give the state an owner with a lifetime, such as the daemon, a build or a task, so
that something drops it.

**Allowing it.** `#[expect(blaze::process_static, reason = "<what bounds it>")]` on the static,
or on the `thread_local!` call for one of its keys. A per-thread spare pool states its cap.

## `hash_order_output`

**What it catches.** A hash map's or set's iteration (`iter`, `keys`, `values`, `drain`,
`into_iter`, and their forms) whose order reaches output, with no sort in between:
- It is collected into a `Vec`, `VecDeque` or `String` that is returned, stored, or handed to a
  call, directly or through its local.
- That local, never sorted, has an element picked by its place (`first`, `last`, `get`, `pop`),
  or is looped over by a loop that writes, hashes or pushes.
- A `for` loop over it pushes, writes or hashes.
- The chain ends in a method that picks one element by its place in the order: `next`, `last`,
  `nth`, `find`, `find_map`, `position`, or `max_by`, `min_by` and their `_key` forms, which
  keep the first or the last of a tie.
- It is `extend`ed into an ordered collection that is not sorted after.

`default_hasher` is the hasher's choice, not the order's.

**Why it matters.** Rule `deterministic-output`: a hash table's order depends on its hasher and
its history, so the output differs between runs. The picking ends and `extend` were added on
2026-11-13 (lane A, class A7). Each of them took one element, or a whole order, from a table
with no sort, and the lint did not see it.

**The fix.** Sort what comes out, choose by key rather than by place, or keep the data in a
`BTreeMap` or in an ordered `Vec`.

**Allowing it.** `#[expect(blaze::hash_order_output, reason = "..")]` on the item.

## The std mistakes

The calls of std's API that people and LLMs most often get wrong: each compiles, passes a quick
test and fails on the input nobody tried (a short write, a NaN, a multi-byte character, a 4 GiB
file, a thread nobody joins). Each lint is in `blaze::internal`, `deny` in blaze's workspace
through the ratchet, and finds its call by what it resolves to and the types inference gave,
never by its text. Test code is left alone. A fix is machine-applicable only where behaviour is
the same or a panic becomes a refusal the code hands on; any other fix is offered, not made.
Each place that is right says why with `#[expect(blaze::<lint>, reason = "..")]`.

Already found by other lints: a swallowed error (`.ok()`, `let _ =`, `unwrap_or_default` of a
`Result`, `if let Ok` with no `else`, `map_err(|_| ..)`, `filter_map(Result::ok)`, a `flatten`
of `Result`s) is [`dropped_error`](#dropped_error); the ambient clock and `thread::sleep` are
[`wall_clock`](#wall_clock); a `collect` only counted, tested for emptiness or iterated is
[`collect_then_consume`](#collect_then_consume); `dbg!` is clippy's `dbg_macro`;
`mem::forget` is `.clippy.toml`'s ban, which `forget` follows; a narrowing `try_from` of a
length is [`narrowed_len`](#narrowed_len).

## `partial_write`

**The mistake.** `w.write(buf)?;` with the count thrown away. `write` may write part of the
buffer (a pipe, a socket, a full disk) and says how much; the rest is lost without an error.

**The right form.** `w.write_all(buf)?`. The fix renames the call, machine-applicable: the count
was unused, and a short write becomes an error. A loop on the count is not a finding.

## `partial_cmp_unwrap`

**The mistake.** `a.partial_cmp(b).unwrap()` (or `.expect`) on floats, mostly as
`v.sort_by(|a, b| a.partial_cmp(b).unwrap())`: one NaN panics the sort.

**The right form.** `a.total_cmp(b)`, offered (it orders `-0.0` before `+0.0` and puts NaN
last where the code panicked), or a typed refusal of NaN before the comparison.

## `unbounded_read`

**The mistake.** `read_to_end`, `read_to_string`, `fs::read`, `fs::read_to_string`,
`Command::output`, `BufRead::lines` and `read_line` with no limit: whoever writes the input
chooses how much memory the process takes. A reader through `.take(limit)` is not a finding.

**The right form.** `reader.by_ref().take(limit).read_to_end(&mut buf)?` and a refusal when the
limit is reached; `fs::metadata(path)?.len()` checked against a cap before `fs::read`; a child
spawned with piped output read through `take`. No fix: the limit is the caller's.

## `len_minus`

**The mistake.** `v.len() - 1`, `buf.len() - header` in an unsigned type with no check before
it that the length is the larger: a debug build panics, a release build wraps to near
`usize::MAX`. The left side is std's `len` (resolved, never a name), called there or the
initializer of an immutable local it names. Not a finding: a right side that is `0`; a right
side `n.min(len)` or `min(n, len)`; a dominating guard, in an enclosing condition or an earlier
statement (`assert!` included), that compares the same length with the same right side (same by
resolution), checks `is_empty` for a literal right side, or calls `checked_sub` on the two.
`implicit_overflow` leaves exactly these findings to this lint while it is on.

**The right form.** `checked_sub` with a typed error naming both lengths, the check before, or
`saturating_sub` where zero is meant. In a function returning `Option`, the fix
`len.checked_sub(n)?` is offered.

## `str_byte_slice`

**The mistake.** `&s[a..b]` or `s.split_at(n)` on a `str` at an index nothing says is on a
character boundary: it panics inside `é` or an emoji. Not findings, all decided by resolution:
a constant index, std's `s.len()`, an index made by std's `find`, `char_indices`, `len_utf8`,
`floor_char_boundary` and the like (directly or through the local it names), and an index a
dominating `s.is_char_boundary(i)` on the same string and index checks (an enclosing condition
or an earlier statement, `assert!` included).

**The right form.** `s.get(a..b)` or `split_at_checked(n)` with the `None` handled. In a
function returning `Option` the fix writes `s.get(a..b)?`, machine-applicable: the panic becomes
a `None` handed on.

## `forget`

**The mistake.** A `ManuallyDrop::new(x)` the function throws away or never ends, hands on or
drops; a `Box::leak` outside a `static`'s initializer for a type that is not
`#[blaze::program_lifetime]`. What it holds is never given back.

**The right form.** End it with `ManuallyDrop::drop`, `into_inner` or `take`; keep the `Box`
with its owner. No fix.

## `path_to_str_unwrap`

**The mistake.** `Path::to_str().unwrap()` or `OsStr::to_str().unwrap()`, which panic on a name
that is not UTF-8, and `to_string_lossy()` outside a `Display` or `Debug` impl, which changes
such a name silently so it names another file or merges two keys.

**The right form.** Keep the `Path` (`join`, `as_os_str`, `as_encoded_bytes`) where it goes back
to the OS or into a key; `to_str().ok_or_else(..)?` with an error naming the path where UTF-8 is
needed; `.display()` where it is shown. No fix.

## `detached_thread`

**The mistake.** `thread::spawn(..)` or `Builder::spawn(..)` whose `JoinHandle` is dropped or
bound and never named: a panic in it is lost, and the process can exit with it half done.

**The right form.** Keep the handle and `join` it, or `std::thread::scope`. A handle returned,
passed or stored is not followed. No fix.

## `unbounded_channel`

**The mistake.** A channel with no bound: `mpsc::channel`, `crossbeam_channel::unbounded` (and
through `crossbeam::channel`), tokio's `unbounded_channel`, `flume`, `async_channel`, `kanal`
and `futures`' `unbounded`. A fast sender grows it until memory runs out. Reported through the
`disallowed_methods` table under this lint; a configured entry for the same path wins.

**The right form.** `crossbeam_channel::bounded(n)`, `mpsc::sync_channel(n)`, so a full channel
pushes back. No fix: the bound is the author's.

## `transmute`

**The mistake.** `mem::transmute` or `transmute_copy`, which check only the sizes.

**The right form.** std's safe function for the pair, which the fix writes, machine-applicable
for numbers (each is defined as that transmute): `to_bits`, `f64::from_bits`, `cast_signed`,
`cast_unsigned`, `to_ne_bytes`, `from_ne_bytes`, `u8::from(bool)`, `u32::from(char)`. Raw
pointers get `p.cast()` offered (not for fat pointers). Every other pair: `bytemuck` or an
`#[expect]` with its reason.

## `clone_to_satisfy`

**The mistake.** A clone of a `String`, `Vec`, `Box`, map, set or path only borrowed at once:
`f(&x.clone())`, `x.clone().len()`. The whole value is copied for one read.

**The right form.** Borrow it: `f(&x)`, `x.len()`. The fix of `&x.clone()` as an argument of a
call that names `x` nowhere else is machine-applicable; the rest are offered, as the longer
borrow may meet a later `&mut x`. `let y = x.clone();` used only by reference is not found
(`redundant_clone` finds a clone never used again).

## `unchecked_index_in_loop`

**The mistake.** `for i in 0..v.len()` whose body only indexes `v[i]`, indexes another sequence
`w[i]` with nothing saying it is as long, or reaches `v[i + k]` / `v[i - k]` past the range.

**The right form.** `for item in v.iter()`, `v.iter().zip(w)`, `v.windows(k + 1)`, or `get`.
The fix rewrites the plain shape (every use `v[i]` read, `&v[i]`, `v[i].f`, `v[i].m(..)`) to
`for item in v.iter()`, machine-applicable when `v` is a local, offered when it is a field.

## `default_numeric_fallback_len`

**The mistake.** `let mut n = 0;` that inference makes an `i32`, then `n as usize` where it is
used: a length or index in the wrong type, overflowing at 2^31.

**The right form.** `let mut n: usize = 0;` and no casts, which the fix offers (an `i32` use of
it would no longer compile, and a value below zero now underflows).

## `refcell_borrow_across_call`

**The mistake.** A `RefCell` guard held while the function borrows the same cell again (either
borrow mutable), or calls something it hands the cell or what holds it (`self`): a
`BorrowMutError` panic on re-entry. Within one function and by the cell's written place: a call
reaching the cell through a global, a thread-local or another name of the `Rc` is not seen.

**The right form.** End the guard first (`drop(g)`, a block), or copy out what the call needs.
No fix.

## `process_exit`

**The mistake.** `std::process::exit` or `abort` outside a binary's entry module (its crate
root): no destructor runs, so buffered writers, temporary files and lock files are left as they
are, and a library ends a process it does not own. Reported through the `disallowed_methods`
table; a configured entry for the same path wins, as blaze's own `.clippy.toml` bans both
everywhere.

**The right form.** Return from `main` (`-> ExitCode`, or `-> Result<(), E>` with a typed
error); an `abort` where nothing can unwind says so with `#[expect]`. No fix.

## `print_in_lib`

**The mistake.** `print!`, `println!`, `eprint!` or `eprintln!` in library code (a library
target, or a binary's modules but its crate root). The stream is the program's: it may be a
pipe or a protocol one stray line breaks, and nothing can match on or silence a print. Where
clippy's `print_stdout` or `print_stderr` is on, the place is left to it.

**The right form.** Hand the caller what it shows (a value, a typed error), take a
`&mut impl Write`, or emit a `tracing` event. No fix.

## `lock_unwrap`

**The mistake.** std's `Mutex::lock()`, `RwLock::read()` or `write()` then `.unwrap()` or
`.expect(..)`: after one panic inside the guard the lock is poisoned and every later caller
panics too. Only where no configured ban of the lock type reports it already (blaze's
`.clippy.toml` bans std's locks for `blaze_core::lock`).

**The right form.** A lock that does not poison (`parking_lot`, `blaze_core::lock`), or the
`PoisonError` handled: `into_inner` where the data is whole after a panic, a typed error where
it is not. No fix.

## `dropped_error`

**What it catches.** An error thrown away for a value: an `Err(_)` arm that makes a value,
`.ok()`, `.unwrap_or(..)`, `.unwrap_or_default()`, `.unwrap_or_else(|_| ..)` and
`.map_err(|_| ..)` of a `Result`, `let _ = <Result>`, `if let Ok(..)` with no `else`,
`.filter_map(Result::ok)`, and `.flatten()` of an iterator of `Result`s. A place a clippy port
reports when it is on (`unused_result_ok`, `map_err_ignore`, `let_underscore_must_use`) is left
to it.

**The right form.** Hand the error on with `?`, or keep it as the new error's source.

## `wall_clock`

**What it catches.** `Instant::now`, `SystemTime::now`, their `elapsed`, and `thread::sleep`,
outside a module a `clippy.toml` entry's `allow-in` names (a driver).

**The right form.** A core reads time from its injected clock and waits for a deadline on it
(AGENTS.md rule 14); the driver reads the wall clock and sleeps.

## Suspicious code

Code that compiles and reads as right but leaves something implicit, which the next reader (a
person, or an LLM editing it) has to reconstruct and gets wrong: an early exit that skips the
second half of a pair, a value left half changed by a `?`, a conversion whose target only
inference knows, a fallback that hides the failure it answers, a test that checks nothing. Each
lint is in `blaze::internal` (crates/blaze-lints/src/blaze/suspicious/, one module each) and
allowed in other workspaces until they turn it on. In blaze's workspace each is at `warn` in
`lints-ratchet` until it has a baseline (the tree cannot yet run `blaze check` on itself to make
one, and `deny` with none would fail the first run); once baselined each goes to `deny`, except
`half_done`, `option_question_mark_hides` and `shadow_changes_meaning`, which stay at `warn`
because many of their findings are a judgement, not a bug. Each is
found by name resolution, inferred types or the MIR's control flow, never by text.

An `#[allow]` with no reason, and any `#[allow]` where `#[expect]` would say when it stops
suppressing, are clippy's `allow_attributes` and `allow_attributes_without_reason`, which
`blaze check` implements and the workspace turns on; there is no blaze lint for them. Clippy's
`wildcard_imports` and `shadow_unrelated` are not implemented by `blaze check`; `glob_import`
and `shadow_changes_meaning` below are their blaze forms.

## `skipped_pair`

**What it catches.** The first call of a pair whose second, on the same receiver, does not come
after it on every path: `begin`/`end`, `start`/`stop`, `open`/`close`, `enter`/`exit`,
`push`/`pop`, `acquire`/`release`, `lock`/`unlock`, or a function marked
`#[blaze::pairs_with(second)]`. A `?`, `return` or `break` between the two leaves with the first
done and the second not: `self.scopes.push(s); let t = self.resolve(e)?; self.scopes.pop();`
leaves the scope pushed on every error. Read from the MIR: the receiver is the place the first
argument borrows, through reborrows, and both callees are of one type, trait or module; a path
that ends in a call that never returns is no skip. The message names the exit. Left alone: a
first whose second the body never calls, a first that returns a value with drop glue (a guard),
test code.

**The right form.** A type: a guard whose `Drop` does the second, or a `#[blaze::linear]` value
the second consumes, so the compiler checks every path. No fix.

## `half_done`

**What it catches.** A function returning `Result` or `Option` that writes through a `&mut`
parameter (`&mut self` too), can then leave by `?` or `return`, and writes the same parameter
again after: on that exit the caller has the error and a half-changed value (`self.len += 1;
let slot = self.alloc()?; self.slots.push(slot);`). A write is an assignment through the
parameter or a `&mut` borrow of a place behind it. The message names the first write, the exit
and the later write. Left alone: a function taking its value by value (a builder consumed and
handed back), closures and `async` blocks, test code.

**The right form.** Compute everything that can fail into locals first and write after the
last exit, or change a copy and swap it in; `fn f(self) -> Result<Self, E>` makes it a type.
Warns in blaze's workspace (many findings are deliberate: a counter bumped before a fallible
call); no fix.

## `hidden_exit`

**What it catches.** A call of a `macro_rules!` or `macro` of a workspace crate whose expansion
leaves the caller: a `return`, a `?`, or a `break`/`continue` outside a loop of the expansion.
Exits inside the expansion's closures, nested functions, `async` and `try` blocks do not count,
nor exits in the arguments the caller wrote (`log!(f()?)`). Macros of std, core and dependencies
are left alone, and test code.

**The right form.** Have the macro make the value and write the `return` or `?` at the call
(`return Err(error!(..))`). No fix.

## `unused_parameter`

**What it catches.** A parameter of a function of the workspace that its body never reads, a
name starting with `_` included: the underscore only quiets rustc's `unused_variables`. Read
from the HIR with macro expansions (a parameter named only in `format!` is read), closures
included; a write is not a read, a compound assignment is. Left alone: `self`, a trait's method
or an impl of one, a function with an ABI, a function under `#[cfg(..)]` (a twin of another
target's), a body that diverges, a signature a macro wrote. A callback is reported: that a
function is passed as a `fn` pointer is a fact of other bodies, and each body's lint reads only
its own.

**The right form.** Read it again where the body lost its use, or remove it with its argument
at every call in one change; a signature fixed for a reason (a callback's) says it with
`#[expect(blaze::unused_parameter, reason = "..")]`. No fix.

## `versioned_name`

**What it catches.** An item named another's with a version on it (`parse_v2`, `load_new`,
`read_old`, `encode_legacy`, `build_tmp`, `lower2`, `ParserV2`, `StoreOld`) while the item of the
plain name is still declared beside it: in the same module, or among the same type's associated
items in the crate. Two versions of one thing, and neither name says which is current. A name
that only looks like a version (`sha2`, `level1`) is left alone while no item of the stem is
beside it, as are test code and an item (or twin) marked `#[deprecated]`.

**The right form.** One item under the plain name: move the callers with a `.clippy.toml` ban
and `blaze check --replace OLD=NEW`, then delete the other; or, when both stay, name each for
what it does differently (`parse_strict`, `parse_lenient`). No fix.

## `glob_import`

**What it catches.** A private `use path::*;`: every public name of `path` comes into scope
unseen, a name in the file does not say where it is from, and an item added to `path` later can
shadow one or change which method a call resolves to. Left alone: `pub use` (a re-export), a
glob of a module named `prelude`, `use super::*;` in test code, and a glob of an enum's variants
inside a function body (for a `match`).

**The right form.** Import each name the scope uses. The fix (machine-applicable) writes that
list, each name found by name resolution, each trait of the module kept as `Trait as _`; a name
seen only inside a macro call's tokens is kept too and makes the fix maybe-incorrect.

## `shadow_changes_meaning`

**What it catches.** A `let` that reuses the name of a binding still in scope for a value not
made from it: `let len = header.len(); .. let len = body.len();`. A line below reads the second
where the first was meant, and with one type nothing catches it. The shadowed binding is the one
the resolver names at the initializer's scope; a shadow whose initializer reads it (`let x =
x.trim();`, `let path = path?;`) is the same thing narrowed, and left alone. Also left alone:
names starting with `_`, bindings a macro made, `match` arm and closure bindings, test code.

**The right form.** A name for each value (`header_len`, `body_len`), or a block that ends the
first one's scope. No fix.

## `fallback`

**What it catches.** A `Result`'s failure answered by computing the same result another way
without looking at the error, so the second route runs on every failure, not the one it was
for (AGENTS.md rule 3): an `Err(_)`, `Err(..)` or `Err(_name)` arm whose value is a call with
arguments (or a method call other than `clone`, `to_owned`, `to_string`, `into`, `cloned`,
`copied`); `.unwrap_or_else(|_| route(..))`; `.or_else(|_| ..)`; and `if let Err(_) = r`, a unit
`Err(_)` arm, `if r.is_err()` or `if !r.is_ok()` whose branch assigns a call's value to a place.
Left alone: arms that return, break or panic, an `Err(e)` that reads `e`, a constant default
(`Err(_) => 0`, which is `dropped_error`'s), test code. Where both are on, a place is
`fallback`'s and `dropped_error` passes it over.

**The right form.** Hand the error on with `?`, or match the one failure the second route is
for (`Err(e) if e.kind() == NotFound => route(..)`) and hand on the rest. No fix.

## `unseen_target`

**What it catches.** `Into::into`, `TryInto::try_into`, `str::parse`, `Iterator::collect`,
`Iterator::sum` and `Iterator::product` with no turbofish, and `Default::default()`, whose
value initializes a `let` with no type written (through parentheses, `?`, `.unwrap()` or
`.expect(..)`): the type is fixed only by a later use. Left alone: the value as an argument, a
return, a struct field or an assignment to a typed place, where the type is one step away.

**The right form.** The type written where the value is made. The fix writes a turbofish for
`parse`, `collect`, `sum` and `product` (`.parse::<u32>()`) and the `let`'s type for the others
(`let w: Wide = n.into();`): machine-applicable when the type's text has no `::`, maybe-incorrect
when it does, none when it has a lifetime, a closure or an `impl`.

## `question_mark_converts`

**What it catches.** A `?` whose operand's error `E` is not the function's error `F` and is
converted by a `From` no one sees, when (a) `E` is a std error that names no subject (`io::Error`,
`fmt::Error`, `ParseIntError`, `ParseFloatError`, `ParseBoolError`, `ParseCharError`,
`Utf8Error`, `FromUtf8Error`, `FromUtf16Error`, `TryFromIntError`, `TryFromSliceError`,
`AddrParseError`, `VarError`): "no such file" arrives without the path; or (b) `F` is erased
(`Box<dyn Error>`, `String`, `anyhow::Error`, `eyre::Report`): `E`'s type is gone. Left alone:
an operand already of type `F`, test code, `async` and `try` blocks.

**The right form.** `.map_err(|source| Error::Read { path, source })?`: a variant that carries
the subject. No fix.

## `option_question_mark_hides`

**What it catches.** In a function or closure returning `Option`, a `?` on a lookup's `None`
(`get`, `find`, `find_map`, `first`, `last`, `nth`, `position`, `strip_prefix`, `split_once`,
`parent` and the like) after an earlier lookup's `?` in the same body. The first `None` means
"not there"; a later one means a broken invariant, and the caller cannot tell them apart. The
message names the first lookup. Left alone: the first lookup, `.ok()?` (`dropped_error`'s), test
code.

**The right form.** Return a `Result` and answer the later miss with
`.ok_or(Error::Dangling { key })?`, or `.expect("why it cannot miss")`. No fix.

## `placeholder_body`

**What it catches.** A function whose whole body is a constant success (`Ok(())`, `Ok` or `Some`
of a constant, `None`, `true`, `0`, `()`, a no-argument `new()`, `default()` or `empty()`) while
it has inputs it ignores (parameters, or a `self` with fields) or its doc promises an effect
(writes, sends, saves, removes). It reports success for work it did not do. Left alone: a
trait's default body and test code; a trait impl's method only when it repeats the trait's
default body, and then the fix (machine-applicable) deletes the override.

**The right form.** Do the work, or make the hole loud: `todo!()` or a typed error naming what
is not built.

## `test_asserts_nothing`

**What it catches.** A `#[test]` with no way to fail: no call that never returns (`panic!`,
`assert!` and what is built on them), no `unwrap`/`expect` of an `Option` or `Result`, no `?` or
`return` in a test returning `Result`, no `#[should_panic]`, and no helper of the crate that has
one (looked into one level deep). It passes whatever the code does.

**The right form.** `assert_eq!(got, want)`, a returned `Result`, or
`#[should_panic(expected = "..")]`. No fix.

## `assertion_cannot_fail`

**What it catches.** In any body: `assert!(true)` or `assert!(CONST)`, `assert_eq!` with one
place on both sides (`x`, `self.len`), `assert_eq!` or `assert_ne!` of two literals, and an
assertion after a statement of type `!` in its block (never reached). `assert!(false)` is left
alone.

**The right form.** Assert what the code computes against what it must be, or delete the
assertion. No fix.

## `test_misses_its_subject`

**What it catches.** A `#[test]` that calls, builds or names nothing of its crate's non-test
code (test helpers followed one level; for a `tests/` target, any workspace crate it depends on
is its subject). It checks a copy, a mock or std, and stays green whatever the crate does.

**The right form.** Call the real code the test is named for, or delete the test. No fix.

## `ignored_without_reason`

**What it catches.** `#[ignore]` or `#[ignore = ""]` on a test (inside `cfg_attr` too): no one
can tell whether it is slow, broken or forgotten.

**The right form.** `#[ignore = "why, and what would let it run"]`. No fix.

## `doc_names_nothing`

**What it catches.** A path in single backticks in a doc comment (`a::b`, or one `CamelCase`
name) that resolves to nothing, resolved as rustdoc resolves an intra-doc link from the item's
scope, with no visibility asked (a private item of another module, a test in `mod tests`, is
named all the same). Left alone: lowercase words, macros, generic parameters, primitives, link text, code
blocks, and a bare name a dependency exports. A doc that names a renamed or deleted item sends
the reader (and an LLM) to look for it.

**The right form.** The path of an item that exists, or plain words for what is not an item.
When exactly one item of the crate has the name, the fix (maybe-incorrect) writes its path.

## `magic_number`

**What it catches.** A number in code (an argument, an operand, a returned or bound value, a
range bound, an array repeat count) that is neither derived from what it depends on nor a named
`const`. Read on the typed tree, by the literal's token, value and type; four findings, the
strongest first:

- *Disagreeing*: a slice of 3 bytes read as a `u32` (`from_le_bytes`), or an array indexed by
  an enum whose length is not the enum's variant count.
- *Derivable*: the length of the fixed array a `for` range or a bound walks (`0..4` over `[T;
  4]`, the fix `a.len()`, machine-applicable), an enum's variant count where the array is
  indexed by it (`core::mem::variant_count::<E>()`, machine-applicable where the crate has
  `variant_count` on, else maybe-incorrect), the size of the integer bytes are read into
  (`size_of::<u32>()`), or the value of the one `const` passed at the same parameter elsewhere
  in the crate (the fix writes its name, maybe-incorrect).
- *Repeated*: the same value and type at the same callee parameter or compared with the same
  field, static or method in two or more bodies; the message lists the bodies and proposes a
  name from the parameter (`const CAPACITY: usize = 4096;`).
- *Plain*: any other; a `Duration::from_*` argument is told "name this duration".

Left alone, with why:

- `const`, `static`, enum discriminant and inline `const { }` initializers: that is the name.
- `0`, `1`, `-1`, `0.0` and `1.0`: identities, which no name explains better.
- `2` as an operand of `*`, `/`, `%`, a shift or `pow`: doubling and halving say themselves.
- A tuple index, an array index or slice bound a fixed array's length proves in range, and
  `0..`: the type already states the bound.
- Literals in patterns (`0x7f =>`, `b'0'..=b'9'`): a pattern is a case of the data's format,
  named by the arm; a closed set of string patterns is `magic_string`'s.
- Byte, char and string literals: strings are `magic_string`'s; a byte or char spells itself.
- Test code (a `#[test]`, a `cfg(test)` module, a test, bench or example target): fixture
  data.
- Attribute and macro arguments, and anything a macro wrote: the macro owns the number (a gap
  for `vec![0; 4096]`, named below).

Not yet seen: array lengths written in types and signatures (`[u8; 24]` in a field), literals
inside macro arguments, and a `const` initializer that repeats another `const`'s value.
`retry` reports retry bounds of its own; `many_bools` and `bool_param` are the boolean
siblings.

**The right form.** Derive the number from what it depends on, else a `const` whose doc
comment says why the value is what it is, else a field of the typed settings. In
`blaze::internal`; denied in blaze's workspace through its `internal = "deny"` ratchet
(`baselines/blaze.magic_number.txt`).

## `magic_string`

**What it catches.** A string literal that names something the program branches on or looks up
by, which is a fact with one home: an enum where the set is closed, else a named `const`, else
a field of the typed settings. Its findings, the strongest first:

- *Whole set*: a `match` with one string arm per variant of a fieldless enum, or an array
  holding the name of every variant (of at least the set floor): with `blaze_comptime` on,
  derive the names with `for const` over the variants; otherwise keep one `const` table
  beside the enum, indexed by its discriminant. Its members are not reported one by one.
- *Named already*: a literal whose text a `const` of this crate already holds, or, for a
  key-like literal (or a const named after its own value, `const BLAZE_HOME: &str =
  "BLAZE_HOME"`), a `const` of a local dependency: "`dep::STORE_VAR` already names this; use
  it", with a machine-applicable fix when the const is nameable from here, and "make it
  `pub`" when it is not.
- *Item name*: a literal that spells a variant, field, function, the enclosing type's or the
  crate's name (`env!("CARGO_PKG_NAME")`) where a rename would not follow it
  (`Kind::Lib => "lib"`, `"dep-info"` for `DepInfo`): derive it by reflection,
  `core::any::type_name` or `env!`. The message says the case convention the literal is in
  (lower, snake, kebab, screaming snake, camel, Pascal). An environment variable's or a
  path's name is never the enclosing function's, type's or crate's: it is fixed outside the
  program and they are named after it (`fn home() { env::var("HOME") }` is a lookup).
- *Near miss*: two literals in the same role (the same callee parameter or compared place) in
  different bodies that differ only by case, `-` against `_`, a plural `s`, or one non-digit
  character when both are 6 characters or longer (`"dep-info"`, `"dep_info"`); both sites are
  reported. Shorter words one letter apart (`"get"`, `"set"`) and numbered names
  (`"worker1"`, `"worker2"`) are two names, not a typo.
- *Repeated*: the same trimmed literal in two or more places in the crate outside tests, other
  than message text; the message lists the bodies.
- *Closed set*: a place matched against string patterns, or compared with `==` against two or
  more literals in one body: "make it an enum with these variants", listing them; its members
  are not reported again one by one.
- *Lookup*: a literal compared with a value, used as a `HashMap`, `HashSet`, `BTreeMap` or
  `BTreeSet` key, read as an environment variable (`std::env::var`), or joined to a path.

Left alone, with why: message text for people (formatting and diagnostic macro arguments,
`expect`, `context`, doc strings), a literal with whitespace or no letter or digit (prose,
punctuation the code writes), the empty string, a literal in a `const` or `static`
initializer (that is the name), and test code. Near misses are compared across bodies only.

`string_key` sits beside it: that lint is about a map keyed by an identifier's text
(`String` where a `Symbol` or an id belongs); `magic_string` is about the literal. A large
string `match` is also `fixed_key_lookup`'s, for its speed.

**The right form.** An enum where the set is closed (a newtype over an interned name where it
is open), else a `const`, else a field of the typed settings. A fix only for *named
already*: use the `const`. In `blaze::internal`; denied in blaze's workspace through
`baselines/blaze.magic_string.txt`. Whole sets overlap the derive-by-reflection lints
(hand-written `Display` or name tables); where those report a set, they own it.

## `repeated_format`

**What it catches.** One sentence written as a format template in two or more places of a
crate outside tests: the template of `format!`, `write!`, `panic!`, `anyhow!`-style and
blaze's error macros, read from the `format_args!` the macro expands to, with each
placeholder made one (`"{}: cannot read {path}"` and `"{0}: cannot read {1:?}"` are the same).
Only templates of at least `TEMPLATE_WORDS_FLOOR` (3) words outside placeholders count; below
that a template is a label or a layout (`"{} ms"`, `"{}: {}"`), so a two-word sentence is a
miss, not a false finding.

**The right form.** One owner for the sentence: a variant of a typed error whose `Display`
writes it, naming the error type when every site sits inside `Err(..)` of one type, else one
function that formats it. No fix.

**Gaps.** A template built with `concat!` or held in a `const`, and macros that format
without `format_args!` (tracing's field syntax), are not read. In `blaze::internal`; baseline
`baselines/blaze.repeated_format.txt`.

## `string_shared_across_crates`, `string_near_miss_across_crates`

**What they catch.** A key-like string (a const's value, a compared, matched, map-key,
environment or path literal) spelled in two or more crates of the workspace
(`string_shared_across_crates`), or spelled in two crates differently but equal after
lower-casing, dropping `-` and `_` and a plural `s`, or one character apart
(`string_near_miss_across_crates`). A near miss whose one side writes (`insert`, `set_var`,
`create`) and other side reads (`get`, `var`, `open`) says which crate writes and which
reads: the reader never finds what the writer wrote. Prose and a string in one crate only are
not reported.

**The right form.** One `const` in the crate that owns what the string names (or one both
depend on), used by the others.

**How it runs.** Each crate exports its sorted rows (`magic::shared::string_rows`); the
workspace pass k-way merges them (`merge_strings`). The caller is `crates/blaze-findings`,
not wired yet, so `blaze check` does not emit these today. A literal passed only as a plain
argument (a span or counter name) is not a row yet.

## Code that carries no weight

The shapes code written in a hurry, by an agent most of all, leaves behind: a field nothing
reads, a helper only tests call, a trait with one impl, a function that only forwards, a
parameter every caller passes the same constant, and two same-typed arguments passed in each
other's place. Each is found by name resolution and inferred types, never by text
(`crates/blaze-lints/src/blaze/weight/`).

The lints about how an item is used read the whole workspace. Each body gives use rows
(a field read or written, a variant constructed or matched, a function called or named as a
value, the constant each argument is, an impl of a trait); each crate folds its bodies' rows;
an item's facts are the sum over the crates that can name it: every configuration of its own
crate (its lib and its tests) and every workspace crate that depends on one. Items are joined
across configurations by where they are written, not by definition id. A body is production
code, test code (a test, bench or example target, `#[test]`, `#[cfg(test)]`), or a serde impl
(where a value reaches disk or the wire). The built-in derives have no bodies, so what only
`Debug`, `Clone` or `PartialEq` reads is never read.

All ten are in `blaze::internal`: allowed by default, denied in blaze's workspace through
`baselines/blaze.<lint>.txt`. They add to `blaze::dead` and `blaze::test_only`, which judge a
private item within its crate; these judge what another crate could use, across the workspace.

## `swapped_arguments`

**The mistake.** `copy(dst_path, src_path)` into `fn copy(src_path: &Path, dst_path: &Path)`:
both arguments have the parameters' one type, so the call compiles, and the copy runs
backwards.

Found as Google Error Prone's `ArgumentSelectionDefectChecker` finds it (Rice et al.,
"Detecting argument selection defects", OOPSLA 2017): each argument's name (a local, a path's
last segment, a field, a getter) and each parameter's are split into words; a pair is
reported when the swap is a strict improvement for both (each argument shares more of its
words with the other's parameter than with its own, and at least half), and the two
parameters and the two arguments have one type. Names under three letters and `arg0`-like
names say nothing and are left out.

**The right form.** The arguments in the order the names say; better, a newtype per role or a
struct of named fields, so the swap no longer compiles. The fix swaps the two
(`MaybeIncorrect`: the names may be what is wrong).

## `write_only_field`

**The mistake.** A field the workspace writes and production code never reads: only a
derived `Debug`, `Clone` or `PartialEq`, a test, or nothing reads it. Every value carries its
bytes and every constructor an argument for nothing. A field only a serde impl reads is
"stored and never used": it reaches disk or the wire and nothing reads it back.

Found for a field another crate could read, and for a private field whose only reader is a
comparison derive or a serde impl (`dead` counts those as reads; a private field nothing reads
is `dead`'s). Left alone: `_` fields, `PhantomData`, `#[repr(..)]` ADTs, ADTs deriving `Hash`,
`Ord` or `PartialOrd` (a key's fields are used by being hashed and compared), test code.

**The right form.** Delete the field and its writes (with a format change, for a stored one),
or read it where it was meant to be used. No fix.

## `only_used_in_tests`

**The mistake.** An exported function production code never calls or names, that tests do: a
helper written for its test and kept public. The test proves a function nothing runs.

Left alone: a trait's function or an impl's of one, `main`, `extern` and test functions, a
function with an attribute (a `#[no_mangle]`, a registration), one nothing uses at all.

**The right form.** Move it into the tests' module, or call it from the code it was written
for. No fix.

## `single_impl_trait`

**The mistake.** A trait the whole workspace implements once. An abstraction with one user:
readers go through the trait to find the one body, a change edits two signatures, and a
generic or `dyn` over it buys nothing.

Impls are counted across every crate that can name the trait, each once by where it is
written, so a test's mock is a second. Counted as more than one: a blanket impl, an impl for a
type of another crate (an extension trait), an impl for a simulation's type (the sim side of a
sans-IO core), and a trait that bounds an ADT's type parameter (a strategy chosen by type,
rule 78). Left alone: `unsafe`, `auto` and generic traits, traits with supertraits, test code.

**The right form.** The type with inherent methods; the trait comes when a second
implementation does. No fix.

## `forwarding_fn`

**The mistake.** `fn load(p: &Path) -> Config { read_config(p) }`: a body that calls another
function with its own parameters, in order, unchanged. A second name for one function, which
readers follow two hops through and which drifts from the first.

Found when the one expression is a call by path or a method call on `self`, every argument is
the next parameter by path with no conversion, there is no turbofish, and the callee is
neither a trait's, generic nor the function itself. Left alone: an exported function, a
trait's or an impl's, one with an attribute (`#[inline]`, `#[track_caller]`, a `cfg`), an
`unsafe`, `async`, `const` or `extern` one, one named as a value (a callback), delegation to a
field (`self.inner.f(x)`), test code.

**The right form.** Call the function it forwards to, and delete this one. No fix: the
callers change, in other files.

## `constant_argument`

**The mistake.** A parameter every call in the workspace passes the same constant (a literal,
a `const`, a unit variant or struct), with two calls or more: a choice nobody makes, repeated
at every call.

Left alone: a function named as a value or called by a serde impl
(`#[serde(default = "..")]`), a trait's or an impl's function, `extern` and test functions, a
`_` parameter.

**The right form.** The value inside the function, as a `const` or a local, and the parameter
gone. No fix yet: inlining it edits every call, which may be in other files.

## `always_same_result`

**The mistake.** A function whose every return gives the same answer: the same constant, a
`Result` that is never `Err`, an `Option` that is never `None`. Every caller handles an error
or an absence that cannot happen. Its results are the tail (through blocks, `if` and `match`)
and every `return` outside closures; a `?` returns the residual, so it varies.

Left alone: exported, trait and impl functions, `async` ones, one named as a value or called
by a serde impl, test code.

**The right form.** Return the value: a `const` for a constant, `T` for the `Option<T>` or
`Result<T, E>`. No fix: every caller changes.

## `never_constructed`, `never_matched`

**The mistake.**

- `never_constructed`: a variant of an exported enum no production code of the workspace
  constructs (or a serde impl reads in). An error variant nothing returns is the usual one:
  every `match` handles a case that cannot happen. A variant only tests make is said so.
- `never_matched`: a variant production code constructs that no pattern of the workspace
  names, of an enum other variants of which are matched, never compared with `==` or cast with
  `as`: every `match` sends it to a `_` arm or a binding, so it means what that arm means.

Left alone: a `#[default]` variant, test code.

**The right form.** Delete the variant, construct it where its case happens, handle it by name
where it differs, or merge it into the variant it means the same as. No fix.

## `swappable_parameters`

**The mistake.** Two adjacent parameters of one primitive or string type
(`fn copy(src: &str, dst: &str)`, `fn resize(width: u32, height: u32)`): a call that passes
them in each other's place compiles. `swapped_arguments` catches it at a call whose arguments
have names; this is the declaration that lets it happen. Integers, floats, `char`, `str` and
`String` behind any references; a `bool` pair is `bool_param`'s. One finding per function.

Left alone: a private function with one call site, a trait's or an impl's function, `extern`,
`main` and test functions, `_` parameters.

**The right form.** A newtype per parameter, or one struct of named fields the caller writes
by name, so a swap is a type error. No fix.

## Complexity

How hard a function, a type, a file or a crate's modules are to hold in one's head, by the
measures that best predict defects and the effort to change code. Each lint measures one
thing on the typed tree or the MIR and refuses an item over its bound. The message names the
measure, the value and the bound, and gives one concrete way down: the part to move out, by
its lines and its share of the value. These are what a reader, a person or an agent with a
short context, has to keep in mind at once. Code: `crates/blaze-lints/src/blaze/complexity/`.
Test code (`#[test]`, `#[cfg(test)]`) is not measured.

All twelve are in `blaze::internal`. They are allowed by default and at `warn` in blaze's
workspace until `crates/blaze-lints/baselines/items.tsv` holds every existing item at its
value. That takes a bless, which needs the tree to check itself. Once baselined they go to
`deny`: new code meets the bound, and existing code may only shrink.

**Held values.** `items.tsv` has one line per item over its bound when it was first blessed,
sorted: `measure<TAB>path<TAB>fingerprint<TAB>value`. `path` is the item as the finding names
it (`blaze_lints::blaze::complexity::check`; a file by its module; a cycle by its modules,
space-separated). `fingerprint` is 16 hex digits hashing the item's tokens with trivia and
its own name left out. An item matches a line by both; by path alone after an edit; by
fingerprint alone after a rename or move with no edit. An item over its bound is a finding
when no line matches it, or when its value is over the line's. Each finding's note is its
line (`item baseline line: ..`). A bless runs with no file loaded and keeps, of the lines
found, only the items the old file held, each at the lower of its old and new value, so a
value never rises and no item is added after the first bless
(`crates/blaze-lints/src/baseline/items.rs`). The gate's comparison lists each item that is
new, grew, shrank or is gone, and can print the ten worst by value over bound.

| key | lint | default | where the default comes from |
| --- | --- | --- | --- |
| `function-weight-threshold` | `function_weight` | 15 | SonarSource S3776 default |
| `path-count-threshold` | `path_count` | 200 | Nejmeh's NPATH threshold, PMD's default |
| `live-state-threshold` | `live_state` | 6 | working memory: 4 ± 1 chunks (Cowan 2001), 7 ± 2 (Miller 1956); estimate |
| `effect-breadth-threshold` | `effect_breadth` | 4 | of 6 kinds; estimate |
| `exits-threshold` | `exits` | 5 | estimate |
| `function-lines-threshold` | `long_function` | 100 | SonarSource S138 default |
| `function-statements-threshold` | `long_function` | 400 | about 4 MIR statements per line at the line bound; estimate |
| `file-lines-threshold` | `long_file` | 1000 | SonarSource S104 default; this tree's 90th percentile is 1066 |
| `params-threshold` | `many_params` | 7 | clippy `too_many_arguments`, SonarSource S107 |
| `struct-fields-threshold` | `wide_type` | 20 | estimate |
| `enum-variants-threshold` | `wide_type` | 50 | estimate |
| `nesting-threshold` | `deep_nesting` | 4 | this tree's 95th percentile; SonarSource S134 uses 3 |
| `fan-out-threshold` | `fan_out` | 30 | estimate |
| `module-cycle-threshold` | `module_cycle` | 1 | no cycle |

A value at its bound is not a finding. Lines are lines of code: not blank, and not only a
`//` comment. The estimates are rough cuts from a text-level pass over this tree (52,299
functions; 3,984 files, generated and vendored ones excluded): function lines p50 8, p95 57,
p99 121; nesting p95 4, p99 5; parameters p95 5, p99 7. Revisit them once the lints have
measured the tree for real.

## `function_weight`

**The measure.** Cognitive complexity, by G. Ann Campbell's rules ("Cognitive Complexity: a new
way of measuring understandability", SonarSource, 2017, rev. 2018). These rules track how hard
code is to read better than McCabe's cyclomatic number does, because they charge for nesting
and not for a `match`'s arms. +1 for each `if`, `else if`, `else`, `match`, loop, `let .. else`,
labelled `break`/`continue`, and each run of like boolean operators (`a && b || c` is 2). The
`if`, `match`, loop and `let .. else` each also add the nesting they sit at. Closures nest
without adding. A function that calls itself gets +1. `?`, `return` and macros add nothing.
Read on the syntax as written, not on desugared HIR.

**The way down.** The help names the smallest `if`, `match`, arm, loop or closure that brings
the weight to the bound when moved into its own function, with its lines and share. If no
part is enough alone, it names the largest.

## `path_count`

**The measure.** The acyclic paths through the function's MIR, each loop taken once: as
many tests as a full path cover needs. This is NPATH (Nejmeh, CACM 31(2), 1988) computed on
the control-flow graph, as Ball and Larus count paths (MICRO 1996). Each back edge is cut, a
loop's latches are joined to its exits, and paths from the entry are summed (saturating).
Unwind blocks and `Unreachable` are left out.

**The way down.** Branches multiply the paths after them, so splitting off the most
branching `if`, `match` or loop divides the count.

## `live_state`

**The measure.** The most mutable places (a `mut` or `ref mut` binding, a `&mut` local) the
runtime MIR holds live across one call: what a reader of the call must keep in mind, since
the call may depend on any of them or change them. The bound is set by working memory.

**The way down.** The help points at the call. Finish what is done before it, or move each
piece of state, with the code that changes it, into a function that returns its result.

## `effect_breadth`

**The measure.** How many of six kinds of effect (IO, blocking, syncing to disk, locking,
allocating, panicking) the function's own calls into other crates bring, by hir-ty-pure's
effect summaries. A function that both does IO and takes locks is two kinds of work to
reason about.

**The way down.** Move the calls of the kind fewest calls bring into a function of this
crate.

## `exits`

**The measure.** The early exits (`return`, `?`, a call or macro that never returns) of a
function that holds a value whose own `Drop` finishes something. Types from `core`,
`alloc`, `smallvec`, `thin_vec` and `arrayvec` only free memory and do not count. Each exit
is a place the reader has to check the value is left right.

**The way down.** Return from one place while the value is held, or finish it explicitly in
one place.

## `long_function`

**The measure.** Lines of code from `fn` to the closing brace, and MIR statements (what
macros and generic calls expand to counts there). Size is the strongest single predictor of
defects; it confounds most other measures (El Emam et al., IEEE TSE 27(7), 2001).

**The way down.** The help names the smallest control-flow part that brings the lines to
the bound. Straight-line code is split where it moves from one step to the next.

## `long_file`

**The measure.** A file's lines of code, its `#[cfg(test)]` items left out.

**The way down.** The help names the smallest top-level item that brings the file to the
bound when moved into a module of its own.

## `many_params`

**The measure.** Parameters, `self` not counted.

**The way down.** Group the parameters that travel together into a struct, or make the
function a method of the type most of them describe.

## `wide_type`

**The measure.** A struct's fields; an enum's variants when at least one has data (a
fieldless enum is a list of names, read one at a time).

**The way down.** When three or more fields share a prefix (`cache_hits`, `cache_misses`, ..),
the help names them as a struct of their own.

## `deep_nesting`

**The measure.** The most `if`/`match`/loop/`let .. else` bodies one point of the function is
inside. An `else if` chain is one level, and a closure adds none.

**The way down.** The help points at the deepest body. Return early from the conditions
around it, or move it out.

## `fan_out`

**The measure.** Distinct functions called, the standard library's not counted (efferent
coupling at the function level; Henry and Kafura, IEEE TSE 7(5), 1981).

**The way down.** Move the calls of one concern, with the code between them, into a
function.

## `module_cycle`

**The measure.** Modules of one crate that import from each other: none can be read, moved
or tested without the rest (the acyclic dependencies principle; Lakos, "Large-Scale C++
Software Design", 1996). An edge is a name one module's scope holds of an item another
defines, `use` included. Edges between a module and its ancestors are nesting and are left
out. Every other edge is lifted to the children of the two modules' nearest common ancestor.
Each strongly connected group of two or more modules is one finding at the crate root,
listing its edges with their names.

**The way down.** Cut the edge of fewest names: move what it uses into the module that uses
it, or both into a module neither depends on.

## Hotspots

`scripts/src/bin/hotspots.rs` ranks files by how often they change times how complex they
are: change frequency from `git log --numstat`, complexity from the `function_weight` lines
of `items.tsv` summed per file (Tornhill, "Your Code as a Crime Scene", 2015). It also lists
pairs of files that change together but sit in different modules. It is for deciding where
refactoring pays off, not a gate.

## Correct by construction

The lints of `crates/blaze-lints/src/blaze/construct/`: a check at run time, a convention or a
name standing in for a type (AGENTS.md: type, then lint, then rule). Each finding says what it
found, the type that would carry it ("carry it in a type: ..."), and what that type removes.
All are decided on the typed tree (HIR, inference, name resolution, the crate's field facts,
`hir_ty_pure::reach`'s proof), never on text: a name is the one a binding, field or method has,
and is read only where its type agrees. None looks at test code or at what a macro wrote. All
are in `blaze::internal` (allowed by default) and held at `warn` in blaze's ratchet until a
baseline holds what the tree has today; `unit_mismatch` is a bug wherever it fires and goes to
`deny` then. Another project opts in at the `suggest` step.

## `proof_forgotten`

An `unwrap` or `expect` of std's `Option` or `Result`, or an index, that the body's proof
(`hir_ty_pure::reach`) discharges, of a local that an `if` condition or `match` scrutinee
written before it reads: `if x.is_some() { x.unwrap() }`, `if x.is_none() { return } x.unwrap()`,
`if i < v.len() { v[i] }`. The check's answer was a proof, dropped and asked again with a panic
site. A site discharged by what it is (`Some(3).unwrap()`) has no such check and is left alone.

**Fix** (`MaybeIncorrect`): for `if x.is_some()` (`is_ok`) of a local holding an owned `Option`
(`Result`), every site in the `then` branch, the condition becomes `let Some(x) = x` (`let Ok(x)
= x`) and each site `x`. It moves `x`; a use after the `if` needs `&x`.

## `unit_mismatch`, `primitive_meaning`

- `unit_mismatch`: `+`, `-`, `+=`, `-=` or a comparison of two numbers whose names (a local's,
  a field's, a method's such as `as_millis`, through casts and borrows) carry different units:
  `elapsed_ms > limit_secs`, `head_bytes + body_kib`. The type is `Duration`, or a newtype per
  size unit; a sum of two units is then a type error.
- `primitive_meaning`: a parameter or field of a bare integer or float whose name carries a
  unit of time (`timeout_ms: u64`) or an identity (`node_id: u32`: a last word `id`, `idx` or
  `index` after another). The type is `Duration`, or a typed id (a newtype per thing,
  `la_arena::Idx<T>`). A trait impl's method and a `#[repr]` struct are left alone.

## `sentinel_value`

An `==` or `!=` of an integer with `-1`, with std's `MAX` (resolved, not read), or with `0`
where the other side's name ends in `id`, `idx` or `index`: a value of the range standing in for
"none". The type is `Option<T>`, with a niche (`Option<NonZero<T>>`, a `NonMax` newtype) where
the size matters.

## `state_in_flags`, `bool_pair_state`

- `state_in_flags`: a struct with two or more `bool` fields named for states of one life
  (`started`, `running`, `closed`, `done`, `failed`, ...). The type is an enum of the states,
  or one type per state.
- `bool_pair_state`: a field, parameter or return type that is a tuple of two or more `bool`s
  or an `Option<bool>`, and a `match` on a tuple of `bool`s. The type is an enum naming each
  state that occurs.

## `out_parameter`

A `&mut` parameter, bound by a plain name, every use of which is the place of `*name = ..`,
in a function returning `bool` or `()` (not a trait impl's method). The type is the return
value: `Option<T>`, `T`, or `Result<T, E>`.

## `parallel_collections`

Two distinct `Vec` fields of the same local pushed in two consecutive statements, or indexed
by the same local in one body: two collections kept the same length by convention. The type is
one `Vec` of a struct, or a struct-of-arrays type with one `push`.

## `untyped_index`

An integer field of a non-test struct that the crate's bodies use as an index into one
collection (from `goose_fields`, the crate's field facts) and do not iterate or measure. The
type is a typed index made only by that collection (`la_arena::Idx<T>`, or a newtype with
`Index<ThatIdx>`). A private `usize` or `u64` read only as an index is `wide_index`'s.

## `validated_but_raw`

A function whose body starts with `if` guards that diverge (`return`, a panic), one of which
reads a parameter of a bare number or string type, and whose value is that parameter, raw or
as `Ok(p)` or `Some(p)`. The check's result is thrown away. The type is a newtype whose one
constructor is the check (`impl TryFrom<T> for Valid`); with ranged integers (`Int<LO, HI>`) or
`#[blaze::invariant]` the check becomes the type's, proved at compile time.

## Claims

The lints of `crates/blaze-lints/src/blaze/claims/` (design: docs/v2/plan/design/claims.md): a
promise a doc comment makes is derived by the compiler, checked by an attribute, or points at
what proves it; never only asserted. A reader, an agent above all, takes a doc's word; these
make the word one the tree holds. The one written form of a held claim is a rustdoc section:

```rust
/// # Guarantees
///
/// - The result is sorted by key. Evidence: [`tests::sorted_by_key`]
/// - A step of the model is deterministic. Evidence: Lean `Blaze.Mir.step_deterministic`
/// - A lookup probes at most twice. Unverified: no test counts the probes yet.
```

`# Guarantees` holds a function's promises, `# Invariants` a type's. Each item ends in
`Evidence:` with intra-doc links to tests or oracles (rustdoc renders and checks them) or
``Lean `Name` `` (a theorem of `lean/theorems.json`), or in `Unverified:` with a reason. `#
Safety` is the caller's obligation and asks for none. `scripts/src/bin/invariants.rs`'s ledger
lists every claim with how it is held and its level.

An effect stated in prose is read only in the narrow form: a clause that opens with the phrase
(after `It`, `This`, `This function`), `Never panics.`, `It cannot panic or allocate.`,
`Allocates nothing.`, `is pure`, or a later phrase of a clause that opens with the item's verb
(`Allocates nothing, takes no lock and never panics`), followed by the clause's end; never one a
word narrows (`allocates nothing once warm`, `never blocks without end`, `a warm read
allocates nothing`). The effect walk (`hir_ty_pure::check_generic`) decides it; `nopanic` only
in a crate built with overflow checks.

All six are in `blaze::internal`, so another project opts in, at the adoption ladder's
`suggest` step. blaze's ratchet: `claim_contradicts_facts` deny; `claim_without_evidence`,
`evidence_is_stale` and `safety_without_invariant` deny, held at each crate's count by their
baselines; `claim_is_derivable` warn; `comment_claims` allow.

## `claim_contradicts_facts`

The doc says the function never panics, does not allocate, never blocks or is pure, and the
effect walk finds otherwise: a reachable panic, an allocation, a blocking call, IO. The finding
quotes the walk and places each step of its chain (`first` calls `pick` here, `pick` indexes
here). Also a `# Panics` section on a function the walk proves never panics with overflow
checks on. A function already marked with the attribute is the attribute's lint's. Not built:
`# Errors` against the error set (error sets are not inferred yet), and complexity claims,
which no analysis decides exactly; both are `Evidence:` or `Unverified:` items.

## `claim_is_derivable`

The doc says an effect the walk proves through every call. **Fix** (`MachineApplicable`):
write `#[blaze::nopanic]` (`noalloc`, `nonblocking`, `pure`) where `unmarked_*` writes it,
and delete the sentence when it says nothing else. On a function already marked, the sentence
repeats the attribute and the fix deletes it. The suggestions lane's lints (`could_be_pure`,
the `unmarked_*` edition lints) say the same of a function whose doc says nothing.

## `claim_without_evidence`

A claim held by nothing: an effect sentence the walk can't decide (a call it can't follow),
an item of `# Guarantees` or `# Invariants` with no marker, `Unverified:` with no reason,
`Evidence:` naming nothing. A marker spelled another way (`Tested by`, `Proof:`,
`evidence:`) is one too; **fix** (`MaybeIncorrect`): the one marker.

## `evidence_is_stale`

An `Evidence:` pointer that resolves to nothing from the item (a pointer names a declaration:
modules from the item's module, `crate`, `self` or `super` first, then the item; visibility is
not asked and every configuration of the crate is searched, so a private test in a
`#[cfg(test)]` module is found from the lib; a `use` is not followed), that is not a test (a `#[test]` or a function of `cfg(test)` code: an oracle, a
simulation), or a test none of whose calls, two levels into the crate's own code, calls the
function or makes a value of the type. What part of a result a test observes is the
test-observation lane's; a theorem's name is checked by the ledger.

## `safety_without_invariant`

A `// SAFETY:` comment above an `unsafe` block or `unsafe impl` that names no invariant by a
backticked path that resolves from the item it is in, or names one and does not say where it is
upheld: `upheld by` a path, `upheld above` (the code just before), or in an `unsafe fn`
`upheld by the caller`.

```rust
// SAFETY: `Ring::cap` is a power of two (the `# Invariants` of [`Ring`]); upheld by `Ring::new`.
unsafe { *self.slots.get_unchecked(i & (self.cap - 1)) }
```

With no `SAFETY:` comment it says nothing: that is clippy's `undocumented_unsafe_blocks`.

## `comment_claims`

Advice: a body comment that states what the code can't do where nothing checks it (`cannot
overflow`, `unreachable`, `can't be None`, `can't fail`, `always in bounds`, `can't be zero`).
The finding names the checked form: `strict_add` or `#[blaze::nopanic]`, `blaze::unreachable!()`
(which blaze proves), `let Some(x) = .. else { blaze::unreachable!() }`, an `assert!` the bounds
checks fold into, `NonZero<T>`. A comment on or above code that already checks it (`assert`,
`expect(`, `unreachable!`, `strict_`, `checked_`, `NonZero`) is left alone. The comment is never
rewritten.

## Duplicates

Code written twice, and the copy that was not finished. An agent with a short context writes a
helper that exists already, or pastes a block and misses one rename; these lints find both with
the compiler's own view of code: the parsed tree, with each local renumbered by first use
(Baker's parameterized matching, as in NiCad and Deckard), so `fn f(a: u32)` and `fn g(b: u32)`
with the same body are one shape, and a literal or a path that differs is a different one. Each
message names every copy, with a note at each. Code: `crates/blaze-lints/src/blaze/dup/`.

All are in `blaze::internal`, allowed by default. In blaze's workspace `same_fn`, `same_block`,
`paste_mismatch`, `same_type` and `same_const` deny, each held at its crate's count by a
baseline (`unmeasured` until the tree checks itself), and the rest warn. Cost: one pass over the
crate's bodies, a query per crate (`dup_index`) that sorts their hashes, and its dependencies'
whole-item hashes merged in, so a copy of a dependency's function is found too. About 200 bytes
a function: two 8-byte hashes, the block and row hashes, a 32-word MinHash sketch past the near
floor. Output is sorted by position. Test code is not measured but by `tests_differ_by_literal`.

The precision column is an estimate; nothing has measured it on a tree yet.

| lint | technique | floor | precision (estimate) |
| --- | --- | --- | --- |
| `same_fn` | the whole body and signature by shape | 12 tokens | near 1: equal shapes are one function |
| `same_impl_fn` | `same_fn` where every copy is a trait method | 12 tokens | high; often a default method waiting |
| `same_block` | a block or a run of statements by shape, the outermost only | 64 tokens | high; short blocks are under the floor |
| `near_duplicate` | 6-token shingles, MinHash (32 hashes, 8 bands of 4), then exact Jaccard | 80 tokens, 85% | medium: tables and match arms look alike |
| `paste_mismatch` | CP-Miner: a block by structure whose renaming is consistent but in one place; rows of a run of rows | 30 tokens; 3 renamed places to one kept; 3 rows | medium-high |
| `same_type` | fields (names, types) or unit variants, in order | 3 fields, 4 variants | medium |
| `same_const` | type and value by shape | 8 tokens or a 12-byte string | high |
| `extractable_duplicate` | anti-unification (Plotkin's least general generalisation) of a near pair or a pair of one structure | at most 4 holes, each of one type both sides | high |
| `tests_differ_by_literal` | a test's hash with each literal erased to its kind | 12 tokens, 2 tests | high |
| `impls_differ_by_type` | anti-unification where every hole is a type | at most 4 holes | high |
| `duplicate_across_crates` | a k-way merge of each crate's whole-item hashes (`dup::index::merge_siblings`) | as `same_fn` | as `same_fn` |

## `same_fn`, `same_impl_fn`

"`second` is the same function as `first`, up to the names of its locals". Where the copy is
exported and plain (no generics, `async`, `unsafe` or ABI; simple parameters) the fix makes it
call the other, `{ first(values, cap) }`, as `MaybeIncorrect`: two that must stay apart say why
with `#[expect(blaze::same_fn, reason = ..)]`. `same_impl_fn` names the trait methods and
suggests a default method or a blanket impl.

## `same_block`

"this code (64 tokens) is written 2 times". The note lists the block's free locals: the
parameters of the function it becomes. A block inside a reported block is not reported again.

## `near_duplicate`

"`b` is a near copy of `a`: 91% of their runs of 6 tokens agree", with the lines that differ.
A pair `extractable_duplicate` or `impls_differ_by_type` explains is reported there instead,
one finding per pair.

## `paste_mismatch`

Two forms. A pasted block where `left` became `right` at three or more places and stayed `left`
at one: "`left` is left as it was in a copy that renames `left` to `right` everywhere else";
the fix renames it (`MaybeIncorrect`). And a run of three or more rows of one structure
(`x.a = y.a; x.b = y.b; x.b = y.c;`) where one row repeats the row above in a place every row
has its own: the fix writes the row's own name. A deliberate one says why with
`#[expect(blaze::paste_mismatch, reason = ..)]`.

## `same_type`, `same_const`

A struct or enum with the fields or variants of another, and a constant with the value of
another: one fact with two homes. The fix is by hand: keep one and `use` or alias it.

## `extractable_duplicate`

"`small` and `large` are one function with 2 parameters: the limit (`64` / `256`, u32, in 2
places) and the callee (`read_head` / `read_tail`)". The two bodies are anti-unified: walked
side by side, locals matched by a bijection, each differing subtree a hole, a hole whose pair
of texts repeats counted once. It fires at most at 4 holes, each a literal, path, field or
expression, of the same inferred type on both sides; a statements hole over 20% of the body
or swapped statements is no generalisation. The help writes the signature; there is no
machine fix.

## `tests_differ_by_literal`

"these 3 tests are one test over a table of 3 rows of 2 values": tests whose bodies are equal
with each literal replaced by its kind, found by that hash, reported once at the first. Help:
one test over a table, or a Hegel property where the expected value can be computed.

## `impls_differ_by_type`

"`sum64` is `sum32` with other types: the type (`u32` / `u64`, in 3 places)". Help: one
generic function or a blanket impl, or `for const` reflection over a closed list of types.

## `duplicate_across_crates`

"the same fn is written in crates `a` and `b`; its home is their nearest common dependency".
The workspace pass (`crates/blaze-findings`) runs it over every crate's whole-item hashes; it
is registered here and not wired yet.

**Clippy's copy-paste lints.** `eq_op`, `ifs_same_cond` and `self_assignment` are in clippy's
`correctness`, denied in blaze's table; `if_same_then_else` is denied and `match_same_arms`
warns. `blaze check` does not implement them yet.

## Suggestions

The group `blaze::suggest` (`crates/blaze-lints/src/blaze/suggest/`): where a plain Rust
project gains from a blaze feature or a tighter type, each finding saying what adopting it
buys. It is the second step of the adoption ladder, one setting in the workspace manifest:

```toml
[workspace.metadata.blaze.lints]
step = "suggest"   # rust (the default), suggest, edition, strict
```

`rust` is `blaze::general` at warn; `suggest` adds `blaze::suggest`; `edition` adds
`blaze::edition`; `strict` is blaze's own table (every group at deny, held by the ratchet's
baselines). Taking a step baselines what the new step shows, per lint and crate, so a crate
never gets worse and each fix lowers its count; the report of the next step gives its findings
by lint, how many fix themselves, and the crates by least work by hand
(`blaze_findings::ladder`; the manifest reader and a command that prints it are not wired yet).
Group and lint lines in the same table apply on top of the step. Nothing in a step changes what
the program means: a lint only reports.

Members besides the four below: `could_be_program_lifetime` and `could_be_ref32`. `suggest` is
a kind, like `general` or `edition`: every blaze lint is of exactly one, so no suggestion is in
`blaze::edition`, and each is off by default.

Each lint's class in the findings scheduler (`crates/blaze-findings`, not yet driving `blaze
check`) says when it arrives: with the answer, right after it (cheap), or in the background
(deferred). Any lint at `deny` runs before the answer whatever its class.

## `could_be_narrower`

"`fn level` returns `u64`, but every value it returns is in 1..=300: `NonZero<u16>` holds
them". Every result of a non-exported function (the tail through blocks, `if` and `match`,
and each `return`) is an integer literal, at least two distinct ones, needing fewer bits than
the declared type; the rule is `blaze_findings::fold::Narrow`. Not found: a trait's function or
an impl of one, `async`, test code, an `isize`/`usize` whose values need more than 16 bits, one
that is narrower only by sign or by never being zero. Class: cheap. No fix (every caller's
arithmetic changes). Parameters and fields (every call's argument, every construction) are the
same join over a workspace fold of per-body rows, deferred; designed, not built.

## `could_be_sans_io`

"the module `app::machine` reaches no ambient effect in its 2 functions: mark it a sans-IO
core". Found at the highest such module (the crate root, or one whose parent's code reaches
the clock, a file, a thread, a socket, a process, the environment, stdio, OS randomness, an
async runtime or a syscall), by `sans_io_core`'s walk. Not found: a test, bench or example
target, a module already in a core, one with no function, a file of only `mod` items, a module
over 4096 functions. Class: cheap. **Fix** (`MaybeIncorrect`): `#![blaze::sans_io]` after the
file's inner attributes and `//!` docs (under rustc the crate root also needs
`#![feature(register_tool, custom_inner_attributes)]` and `#![register_tool(blaze)]`).

## `could_be_linear`

"`Drop::drop` of `Writer` calls `flush_pending`, as `Writer::finish` does: a `Writer` dropped
instead of finished by `finish` does that work where no caller sees it fail". A type of the
crate, not `#[blaze::linear]`, with an inherent method in its module that takes `self` by value,
whose `drop` calls a function of the crate that method calls too, or can panic. Help: the type
`#[blaze::linear]` and the method `#[blaze::consumes]` (docs/v2/plan/design/linear.md). Class:
cheap. No fix.

## `could_have_invariant`

"`new`, `set` each start with `assert!(lo <= hi)`: that is an invariant of `Span`". Two or more
functions of one inherent impl, at least one a constructor (no `self`, returns `Self`), the
rest `&mut self` methods, whose first statement is the same `assert!` or `debug_assert!` by
tokens. Not found: a struct with a `pub` field, one with a `#[blaze::invariant]`, test code.
Help: `#[blaze::invariant(..)]` on the struct, in terms of its fields (proved where a value is
built or written, assumed where read), or a ranged integer (`Int<LO, HI>`) for a field's range.
Class: cheap. No fix.

Not here, by design: a never-written `&mut` parameter is clippy's `needless_pass_by_ref_mut`;
a `Vec` that never grows is clippy's `useless_vec`. `could_derive_from` (a hand-written impl
that a derive or reflection gives) is not built yet.

## Test strength

**The rule.** A test is as strong as what it looks at. These seven lints say, before anything
runs, which parts of a function's result no test observes: the static counterpart of mutation
testing (`blaze-mutants` is the dynamic one). They live in
`crates/blaze-lints/src/blaze/test_strength/`, warn in blaze's workspace and are allowed
elsewhere.

**The analysis.** Each test body, its closures and its helpers are lowered from MIR into a small
IR (`lower.rs`). A sans-IO, flow-insensitive fixpoint (`flow.rs`) then follows each value a
workspace call returns, or writes through a `&mut` argument, to the places that can fail the
test. A part is a path of fields, variants and elements ending in the value itself, its
discriminant or its length. A part is observed when it reaches:

- an `assert!` or `assert_eq!`, which is a branch to a block from which no path returns;
- a checking `unwrap` or `expect`;
- a `PartialEq`: derived impls observe every field, hand-written ones are walked;
- a helper that does one of these (summaries go 3 calls deep);
- a dependency's function, such as a snapshot or property runner.

Printing a value, or dropping it, observes nothing. Wherever the analysis cannot see, it counts
the value as observed, so a finding is meant to be true. A per-crate index (`strength_index`)
merges every test's reads by function path. Each function's lints read only its own row (the
interned `StrengthKey`), so an edit to one test re-lints only the functions whose rows changed.
Doc tests are not read yet.

| lint | fires on | message names |
|---|---|---|
| `result_part_unobserved` | a function some test calls whose result has a part no test observes | the function, the parts, the tests |
| `weak_assertion` | an assert on only `is_some`, `is_ok`, `is_err`, `len` or `contains` of a value with a payload | the call, the predicate |
| `mirror_assertion` | an `assert_eq!` whose expected side runs the same code as the side under test | both sides |
| `pseudo_tested` | a function tests call but observe nothing of: its result and `&mut` writes are never checked | the function, the tests |
| `unreached_by_tests` | a public or crate function no test reaches, in a crate that has tests | the function |
| `error_path_untested` | a function returning `Result` whose tests never check which `Err` comes back | the function, its error sites |
| `property_over_examples` | a function with an inverse beside it (`encode`/`decode`, `to_bytes`/`from_bytes`, ...) tested only by examples | the pair |

None has a machine fix; each finding's help says what to assert.

## `result_part_unobserved`

"`parse` is tested by `parses_header` and 2 more, but no test looks at `Header.flags` or
`Header.len` of what it returns". A part counts as observed when any test observes it. Help:
assert on the whole value, or on the part named.

## `weak_assertion`

"`assert!(find(..).is_some())` checks only that `find` returned a `Some`, not which value".
Quiet where there is no payload to check (`Result<(), E>`, `Option<()>`), and on `is_none` and
`is_empty`, which pin the whole value when they hold. The polarity of a check is not followed, so
`!v.is_empty()` is missed. Help: compare with the expected value.

## `mirror_assertion`

"both sides of this `assert_eq!` run `encode`: the test checks `encode` against itself". Fires
when both sides call the same function with the same arguments, or one side calls the other
within 2 levels.

## `pseudo_tested`

"`apply` is called by 3 tests, but none observes anything it returns or writes": any body for
`apply` would pass them. Help: assert on what it returns or writes.

## `unreached_by_tests`

"no test reaches `orphan`". Reach is a walk from every test's calls, through every function they
call, plus trait methods that are dispatched and const and static bodies. Fires only in a crate
that has tests.

## `error_path_untested`

"`load`'s tests check whether it fails, never which error it returns (it returns one at 3
places)". Matching only `Ok`, or `is_err`, does not count; `assert_eq!(f(bad), Err(..))` or a
`match` on the `Err`'s variant does.

## `property_over_examples`

"`encode` and its inverse `decode` are tested only by examples". Quiet once a test draws a
value (Hegel, proptest, quickcheck) and passes it through either function.

## Structural

**The rule.** Code that the type's structure gives is not written by hand (AGENTS.md rules 56,
reflection-first, and 30, one concept in one home). A body that does the same thing to every
field is the field list written again, and a field added later is an edit there that nothing
asks for. The one that does it to every field but one is usually a bug. These ten lints live in
`crates/blaze-lints/src/blaze/structural/`. They are in `blaze::internal`, so another workspace
turns them on itself, as advice it adopts at the adoption ladder's `suggest` step.

**The analysis** (`structural/uniform.rs`) asks one question of each body: is it uniform over
the type's fields or variants? It runs once per file. The body is split at its combinator:

- its statements;
- one operator throughout (`&&`, `||`, `+`, `*`, `^`, `|`, `&`);
- a method chain (`.field(..)` links between a head and `finish`, `.then(..)` and
  `.then_with(..)` links);
- the fields of a record or tuple constructor (`Ok(..)` and `Some(..)` looked through).

Each piece is a **term**. A term's **template** is its tokens with each read of a field written
`#`. The reads that count are `self.f`, `other.f`, a parameter's `.f`, a local bound by
destructuring `let Self { f, .. } = self`, and the string `"f"`. The template is normalized, so
`self.f.clone()`, `Clone::clone(&self.f)` and `self.f` match, and the default forms
(`Default::default()`, `T::default()`, `0`, `false`, `None`, `""`, and `String::new()`,
`Vec::new()` and the other std collections' `new()`) are one. Each term
falls in one of four classes:

- **clean**: it reads one field, or fills the field it reads;
- **crossed**: it fills one field from another;
- **odd**: it reads two fields;
- **frame**: it reads none, such as `f.debug_struct("T")`.

The template most clean terms share is the body's; a tie means none. The verdict:

- **uniform**: every field is covered by that template once, and nothing is odd, crossed or
  missing;
- **almost uniform**: something is wrong, but two fields or more are covered, and more are
  covered than are odd or crossed;
- **not uniform**: anything else.

A field left out on purpose is written `field: _, // left out: <why>` in a destructuring `let`.
This is the one marker, shared with `key_ignores_field`. A property test checks the analysis
(`structural/tests.rs`). A uniform body stays uniform under any order of its terms. Deleting one
term gives almost uniform, naming that field.

**Levels.** In blaze's workspace (`lints-ratchet`):

- `impl_misses_field`, `variant_list_drift` and `conversion_misses_field` are the bug forms,
  meant to be `deny`. Each is held at `warn` until it has a baseline.
- `hand_written_derivable` warns.
- The other six are advice, `allow`.

**Already covered elsewhere.** These clippy lints overlap and are on in the table, but
`blaze check` does not implement them yet: `derivable_impls`, `derived_hash_with_manual_eq`,
`derive_ord_xor_partial_ord`, `non_canonical_clone_impl` and `non_canonical_partial_ord_impl`.
`key_ignores_field` checks a single `Hash`, `Eq` or `Ord` impl, or key builder, that leaves a
field out. `impl_misses_field` does not check those, and adds the checks across two bodies.

**The end state** for every one of these is reflection: `for const f in fields_of::<T>()` over
a struct's fields, and `variants_of::<E>()` with `Variant::NAME` and `Variant::INDEX` over an
enum's variants (docs/v2/plan/design/comptime.md). Until then the fix is a derive, a declarative
macro or a table.

| lint | fires on | fix |
|---|---|---|
| `hand_written_derivable` | an impl whose body is exactly what the derive writes | the derive, impls removed (machine) |
| `impl_misses_field` | a field-wise body almost uniform with a field missing; two bodies of one type that disagree | none |
| `hand_kept_variant_list` | `ALL`, `COUNT`, `fn name`, `fn index` or a names table derivable from the enum | none yet |
| `variant_list_drift` | such a list that misses, repeats, miscounts, misspells or misorders, or a `_` arm for one variant | the list regenerated (maybe-incorrect) |
| `hand_written_codec` | an encoder or decoder of three fields or more, the same call on each | none |
| `field_wise_conversion` | a `From` or `to_*` copying three fields or more into their namesakes | none |
| `conversion_misses_field` | such a conversion that crosses two fields or drops one | none |
| `hand_written_visitor` | one walk (`visit*`, `walk*`, `fold*`) uniform over the fields of three or more types | none |
| `repeated_field_list` | a struct of three fields or more written out in three uniform methods | none |
| `registry_spread` | an enum with five lists or more in a file, two of them facts no rule derives | none |

## `hand_written_derivable`

"`impl Clone for Pair` is what `#[derive(Clone)]` writes: by hand, it must be edited with every
field `Pair` gains". The lint checks exact forms:

- `Clone`: each field `.clone()`d into `Self { .. }`, or `*self` where `Copy` is derived;
- `Debug`: `debug_struct("Name")`, a `.field("f", &self.f)` per field in order, then `finish()`;
- `Default`: every field its default;
- `PartialEq`: `self.f == other.f` joined by `&&`;
- `Hash`: `self.f.hash(state)` per field, in order;
- `Ord`: `self.f.cmp(&other.f)` joined by `.then(..)` in order;
- `PartialOrd`: `Some(self.cmp(other))`.

Generic types are left alone, since the derive's bounds differ. So is `Eq`, whose derive also
asserts every field is `Eq`. A pair is derived together or not at all: `Hash` with `PartialEq`,
and `Ord` with `PartialOrd`. The fix removes the impls and adds the names to the struct's
derive. It is maybe-incorrect when a removed impl holds a comment.

## `impl_misses_field`

Fires on one body: "`encode` of `impl Encode for Row` covers `a`, `b`, `c` and `d` the same way
but leaves out `e`". Only bodies meant to cover every field are checked: `clone_from`, the
codec methods, `merge`, `reset`, `validate`, walks, and a std `Debug`'s `fmt` unless it ends in
`finish_non_exhaustive()`.

Fires on two bodies of one type:

- "`PartialEq` of `Loose` ignores `c`, which its `Hash` covers": two values equal by `==` can
  hash differently. A `Hash` coarser than `Eq` is legal and is not reported.
- `Ord` against `PartialEq`, and `Ord` against `PartialOrd`, in either direction.
- An encoder against its decoder (`encode`/`decode`, `write_to`/`read_from`,
  `to_bytes`/`from_bytes`, `persist`/`restore`, `serialize`/`deserialize`, `save`/`load`).

A derived side covers every field.

## `hand_kept_variant_list`, `variant_list_drift`

An enum's variants listed again by hand. The lint reads these lists:

- in the enum's impls: an `ALL` array, a count (`COUNT`, `LEN`, `N`, ...), and a `match self`
  that gives a string, an integer or a fact per variant;
- a parse that matches strings to variants;
- anywhere in the file, of an enum of any crate: a `const` or `static` string array whose length
  is `E::COUNT`, written out or made by the file's `macro_rules!`.

A names list follows one rule: the variant's own name, snake, lower, kebab or screaming case,
with any shared prefix stripped.

`variant_list_drift` is the bug form. It fires on:

- a list that misses or repeats a variant;
- a count that is wrong;
- a name off the rule that is another variant's or within two edits;
- a table out of declaration order;
- a parse that disagrees with `fn name`;
- a `match self` whose `_` arm stands for exactly one variant: "`fn name` of `Tag` names 22 of
  its 23 variants and leaves `W` to `_`". A variant added later takes that arm silently.

A `_` over two variants or more is a grouping and is left alone.

`hand_kept_variant_list` is the advice: a list that is right today, reported once per enum.

## `hand_written_codec`

An `encode`, `decode`, `persist`, `write_to`, ... (or any method of a trait named `Encode`,
`Persist`, `Serialize`, ...) that covers three fields or more with one call each. The record's
layout belongs in one place (`blaze_format::record!`, or one generic codec over `fields_of`), so
the encoder and the decoder cannot disagree.

## `field_wise_conversion`, `conversion_misses_field`

The conversions read are `impl From<A> for B` and an inherent `to_*`, `into_*` or `as_*` that
takes `self`.

- `conversion_misses_field` fires on a crossed field: "`from` of `impl From<Wire> for Point`
  fills `x` from `w.y`". `x: w.y` counts as crossed only where the source has its own `x` and the
  target its own `y`. It also fires on a field both types have that is never read.
- `field_wise_conversion` is the advice: three fields or more copied straight across.

## `hand_written_visitor`

The same walk method, uniform over its type's fields, written by hand for three types or more
in one file. Reported once, at the first.

## `repeated_field_list`

A struct of three fields or more whose whole field list is written out in three uniform methods
or more of the file. Each is the declaration again.

## `registry_spread`

An enum of this file with five lists or more, two of them facts no rule derives (`fn kind`,
`fn help`). Each variant's facts then live at its position in every list. The form that replaces
them is one row per variant in one table, from which every list is derived.

## `same_behaviour`

**The mistake.** A function written again in another style under another name: `if` where the
first has `match`, an early `return` where it has `else`, `b + a` for `a + b`, one more `let`.
The syntax lints (`same_fn`, `near_duplicate`) miss it because the text differs; the compiled
behaviour does not.

Left alone: two homes for one rule, which drift the first time one is fixed.

**The right form.** Keep the original and call it. Where the two must stay apart, say why with
`#[expect(blaze::same_behaviour, reason = "..")]`. Internal, allowed by default.

The canonical form (`crates/blaze-lints/src/blaze/dup/semantic.rs`, `CANON_VERSION` 1). A body's
runtime MIR is lowered to a small graph and the steps below are repeated to their fixpoint (at
most 8 rounds; one that does not settle is not compared):

1. Thread jumps: a chain of empty `goto` blocks becomes its end; a jump to an empty end block
   (`return`, `unreachable`) becomes that end.
2. Number: blocks in reverse postorder from the entry, unreachable blocks dropped; the return
   place and the parameters keep their slots, other locals are numbered by first appearance.
3. Fold `bool`s: `!` of a constant, `== true`, `!= false`; a switch on `!x` becomes a switch on
   `x` with its arms swapped.
4. Copy-propagate a temporary written once and read once in its block; forward an operation
   whose temporary is only copied whole by a later assignment of its block (`_t = a * k;
   y = _t` is `y = a * k`); drop stores never read. Nothing between may write what it reads.
5. Simplify switches: arms to the `otherwise` target and arms to `unreachable` go; a switch
   with one target left is a `goto`.
6. Sort the operands of operations that commute on their type (integer and `bool` `+`, `*`,
   `&`, `|`, `^`, `==`, `!=` and the overflow-checked `+` and `*`; float `+` and `*`, whose
   NaN payload Rust leaves unspecified; never `-`, `/`, a shift or a float comparison).

Types, callees and constants are hashed by path and value, not by local name. Compared: free
functions, inherent and trait-impl methods and trait default bodies of the crate and its
local (workspace) dependencies, from 6 to 256 statements and terminators; test code, macro
written bodies, `const fn`s that are one constant, and bodies with closures, coroutines,
inline `asm` or a constant the lowering cannot read are not. A pair that `same_fn` already
reports (equal shape) is left to it. Where the lowering cannot tell, it reports nothing.

Not built: `reimplements_library_fn`. A hand-written `is_none` is equal to `Option::is_none` only
once the callee is inlined; that needs a per-instance query in the front end for MIR with small
callees inlined and overflow checks normalised (core builds without them), and an enumeration
of library items by path.

## Trust

The group `blaze::trust` holds a workspace's dependencies (every crate that is not a member)
to what the root manifest grants them, so a compromised or malicious new version of one fails
the build that pulls it in. A dependency may not run `unsafe` code or use an ambient
capability unless `[workspace.metadata.blaze.trust]` grants it by name, with a reason:

```toml
[workspace.metadata.blaze.trust]
serde_json = { allow = [], reason = "parses JSON; no IO" }
libc = { allow = ["unsafe", "ffi"], reason = "the C library's bindings" }
left-pad = { allow = ["fs"], reason = "reads its config", version = "=1.4.0" }
```

The capabilities are `unsafe`, `ffi`, `fs` (files and the standard streams), `net`,
`process`, `env`, `clock`, `thread`, `global_state`, `random`, `build_script` and
`proc_macro`. `version = "=x"` pins the grant: another version is granted nothing until a
person reads what it does and moves the pin. std, core, alloc, bstd, proc_macro and test are
the toolchain's and need no entry. `blaze audit grants` prints the table a workspace needs
today, each capability with its witness as a comment and `reason = ""` left for a person to
write (an empty reason is refused).

What a dependency uses is read off the effect walk: the union over every function a workspace
crate reaches, each capability attributed to the crate where the call into the toolchain (or
the thing itself) is written, never to its caller. `unsafe`, a build script and a proc macro
count wherever they are in the crate. What the walk cannot see (a `dyn` it cannot resolve, a
body that does not lower, a foreign function without a model) counts as every capability, and
the finding says what could not be seen. Each finding names the crate, the capability and one
shortest chain of calls from the workspace to it. The lints run only where the driver read the
root manifest's grants; they are reported once per workspace, at its first library.
Design: [trust.md](v2/plan/design/trust.md).

## `dependency_capability`

**The mistake.** A dependency touches files, opens a socket, runs a process, reads the
environment or the clock, spawns threads, keeps global state, draws randomness, calls foreign
code, has a build script or is a proc macro, and the workspace did not grant it that. With no
table at all, the workspace is told once, naming `blaze audit grants`.

**The right form.** Read what it does there. If it is trusted with it, add the capability to its
`allow` with a `reason`; if not, pin an older version or drop it. `deny` by default.

## `dependency_unsafe`

**The mistake.** A dependency has `unsafe` code (an `unsafe` block, `unsafe fn`, `unsafe impl`
or `trait`, `asm!`, an `extern` block, `#[no_mangle]`, `#[export_name]`, `#[link_section]`)
the workspace did not grant it, reached or not.

**The right form.** Read it; grant `"unsafe"` with a `reason`, or drop the dependency. `deny`
by default.

## `dependency_capability_unreached`

**The mistake.** A dependency's code that nothing the workspace builds reaches yet uses a
capability it is not granted: the next call the workspace adds may reach it.

**The right form.** Grant it before a call does, or keep the workspace from calling it. `warn`
by default.

## `trust_grant_unused`

**The mistake.** A grant for no dependency, for a version the build does not have, or of a
capability its dependency no longer uses: a stale grant lets a future version use it unseen.

**The right form.** Remove what the grant no longer needs. `warn` by default.

## `workspace_unsafe`

**The mistake.** A workspace crate runs `unsafe` code and its root does not say why.

**The right form.** `#![blaze::trust(unsafe, reason = "..")]` at the crate root, or make the code
safe. Internal, allowed by default; blaze's own ratchet warns.
