---
status: plan
summary: The blaze edition. Rust with ten opt-in changes (handles that copy themselves, error unions, view types, records, contexts, effects at the border, values that must be used up, contracts and pattern types, compile-time reflection, `&uniq`), what it removes, the lints that get a crate ready for it, and the order it is built in.
date: 2026-11-09
---
# The blaze edition

Rust has the hard part right: ownership, traits, enums, no null, no exceptions. Its friction
comes from two places. It asks for marks that tell the reader nothing (`.clone()` on an `Arc`),
and it is silent about what the reader needs (does this function touch the disk, allocate,
panic?). The edition fixes both directions.

```toml
[package]
edition = "blaze-2026.11"
```

A crate on an ordinary edition builds exactly as before (rule `superset-of-rust`). The edition is
named after the release that introduced it and is cut rarely; it fixes its Rust syntax year (this
one is Rust 2024 plus what is below) and is separate from which blaze a project pins.

Three tests decide what goes in:

1. **Write what informs.** A mark stays only if it tells the reader something the checker could
   not tell them more cheaply.
2. **Infer inside, declare at the border.** Effects, views and contexts are inferred for private
   functions and written on every `pub` one.
3. **Each addition deletes something**: a pattern, a crate or a tool people use today.

## The ten changes

Each is its own feature attribute first (`#![blaze::handles]`, ...), usable in any crate, and the
edition turns them all on. The spec column is where the design lives; this page does not repeat
it.

| # | change | deletes | spec | size |
|---|---|---|---|---|
| 1 | **Handles copy themselves.** `Handle` marks types whose clone is a second reference to the same thing (`Arc`, `Rc`, channel senders, interned ids). Where a handle would be moved and is used again, it is copied; the last use moves. | `.clone()` on handles, clone-then-move blocks | here | small |
| 2 | **Error unions.** `A \| B` in type position is an anonymous enum, order-free; `?` widens a smaller set into a larger one; `match` on it is exhaustive. | error enums, `From` impls, `anyhow`, `thiserror` | here | medium |
| 3 | **View types.** `fn old_keys(&self.{lru})` says which fields a method borrows. | structs split to please the borrow checker | here | medium |
| 4 | **Records and named arguments.** `{ a: T, b: U }` is a type, equal to any record with the same fields; fields may have defaults. | builders, options structs, one-off structs | here | medium |
| 5 | **Capabilities as parameters.** With [bstd](std.md), a function touches only what it is handed, so one with no `Fs` or `Dir` parameter cannot reach the disk. (A `with` clause to pass them by name is deferred.) | ambient files, clock, network, allocator | [std](std.md) | large |
| 6 | **Effects are what a function may do.** `#[blaze::may(panic, alloc, block, diverge)]` lists a function's effects, and an unlisted one is checked absent; a module's `#![blaze::may(..)]` is the default of the functions in it; pure is derived; ambient, unsafe and may-suspend follow the same form; a function that takes a closure does what the closure does. | `async`/`await`, and the async, const and `try_` twins of every API | [effects](effects.md), [pure-fns](pure-fns.md), [may-suspend](may-suspend.md) | medium |
| 7 | **Values that must be used up.** A `#[blaze::linear]` value cannot be dropped; every path hands it to a function that takes it by value. | forgotten commits, drop bombs | [linear](linear.md) | medium |
| 8 | **Contracts and pattern types.** `requires` and `ensures` on a function; `usize is 1..` puts a range in the type, and `Conn is Conn::Open { .. }` a variant. | preconditions in doc comments, restated checks, `unreachable!()` after a match already made | [contracts](contracts.md), here | medium |
| 9 | **Compile-time reflection.** `for const f in fields_of::<T>()` in ordinary typed code. | most derive proc macros | [comptime](comptime.md) | large |
| 10 | **`&uniq T`.** The unique reference is spelled for what it guarantees. `&mut T` means the same and is the old spelling. | the belief that `&` means immutable and `&mut` means mutable | here | small |

### Rules for the four that have no spec yet

**1. Handles.**
- `Handle` is a marker trait with `Clone` as its supertrait; implementing it promises the clone
  is cheap, cannot fail and yields the same object. `Vec`, `String` and maps are never handles:
  their clones allocate and make a different value, and stay written.
- A use of a handle that would move it, when the handle is used again on some later path, is a
  copy. The last use on every path is a move, so nothing is counted twice needlessly. A closure
  that must own its captures takes a copy of each handle it names.
- No mark at the use (upstream's experiment writes `cfg.use`): it tells the reader nothing. Where
  the cost matters, `#[blaze::norefcount]` on a function, an effect beside `noalloc`, makes each
  implied copy an error naming the line.
- Observable difference: a value lives until its last copy drops, and `Arc::strong_count`,
  `get_mut` and `try_unwrap` can see copies. So it is edition-only, never on ordinary Rust.

**2. Error unions.**
- Members are concrete named types at first; a type parameter in a union is refused (with two
  parameters equal, `T | U` would be ambiguous).
- Layout is an enum with one variant per member, sorted by the member's type identity, so the
  same set is the same type in every crate.
- `Err(e: io::Error) =>` matches by member. `?` converts when the source set is a subset of the
  target's; otherwise the error names the member that is missing.
- **Unions are not only for errors.** `type AnySpec = OsSpec | VmSpec | WasmSpec;` is the closed
  family "one of these", with no wrapper enum to declare and no `From` impls to keep in step.
- **A union implements a trait when every member does, by delegation**: calling a method calls
  the held member's. An associated type of the union is the union of the members' (`Spec::Sandbox`
  of `AnySpec` is `OsSandbox | Vm | WasmSandbox`), so one call maps a family onto another. A
  trait whose methods take a second `Self` by value or reference (`PartialEq`, `Add`) is not
  delegated, since the two values might hold different members; `Debug`, `Display`, `Error`,
  `Clone` and `Hash` are.
- **One arm can handle every member**: `match x { each v => f(v) }` checks `f(v)` once per
  member type and is the delegation rule written by hand, for when no trait fits.

**3. View types.**
- `&self.{a, b}` and `&mut self.{a}` borrow only those fields; two calls whose views are disjoint
  do not conflict. Private methods have their view inferred from the body; a `pub` method writes
  it, since narrowing it later is a promise and widening it breaks callers.
- A view is part of the method's type for the borrow checker only; layout and ABI are unchanged
  (the method still receives the whole pointer).

**4. Records.**
- `{ path: PathBuf, size: u64 }` is structural: field names and types, in any order. It has
  `Clone`, `Debug`, `Eq`, `Hash` and `Default` when its fields do, as a tuple has. To give it
  methods or other traits, name it.
- A record parameter with defaults is how arguments are named:
  `fn open(path: &Path, opts: { create: bool = false, mode: u32 = 0o644 })`, called as
  `open(p, { create: true, .. })`.
- Layout is the fields sorted by name then laid out as `repr(Rust)`, so equal records are one
  type everywhere.

**5. Contexts are deferred.** Capabilities, `Cx`, allocators and evaluators are plain parameters
([std](std.md)); that is the design, and what a function takes is read off its parameter list. A
`with` clause (a parameter passed by name from the caller's scope, so the functions in between
need not list it) would only save typing in the middle of a call chain. It is not built, and it
is reconsidered only if threading turns out to be the main complaint once blaze's own crates are
on bstd. `main` is `fn main(cx: &Cx, io: Io)`.

**10. `&uniq`.**
- `&uniq T`, `&uniq self`, `&uniq expr` and `ref uniq` in patterns are exactly `&mut T`,
  `&mut self`, `&mut expr` and `ref mut`: the same type, so code in the two spellings mixes
  freely, across crates too. `uniq` is a keyword only in those positions.
- What `&` and `&uniq` promise is aliasing: shared, or unique. Whether the target can change
  follows from that (unique: yes; shared: only through a cell or a lock), which is why
  `&Atomic<u32>` can be written to and the old names mislead.
- `let mut x` stays: it says the binding may be reassigned or borrowed uniquely, which is a
  different thing. `*mut T` and `*const T` stay.
- Diagnostics and docs say "shared" and "unique" in every crate; under the gate they print
  `&uniq`.

**8. Pattern types cover enum variants.** (The rest of change 8 is in [contracts](contracts.md).)
- `T is PAT` is `T` restricted to the values that match `PAT`. A range is one case
  (`usize is 1..`); a variant is another: `Provider is Provider::Host(_)`,
  `Option<T> is Some(_)`, `Conn is Conn::Open { .. } | Conn::Draining { .. }`. There is no
  separate "variant type" feature.
- **Same bytes.** A narrowed value has the enum's layout, so `T is PAT` to `T`, and
  `&(T is PAT)` to `&T`, are free and implicit. The other direction needs proof.
- **A constructor has the enum's type** (`Some(x)` is an `Option`), as today. Narrowing happens
  in two places only: where the type is written, and where control flow proves it. In a `match`
  arm or after `if let`/`let else`, the scrutinee, when it is a local that is not reassigned, has
  the type narrowed to that arm's pattern, and can be passed to a function that asks for it.
- **A `match` on a narrowed type needs only its variants**, and a field of the variant can be
  read directly (`conn.socket` on a `Conn is Conn::Open { .. }`): no `unreachable!()`, no second
  match.
- **A unique reference cannot change the variant.** Through `&uniq (T is PAT)` the fields may be
  written, but the whole value may not be assigned or swapped, since that could store a value
  outside `PAT`.
- With error unions: handling one member in a `match` leaves the rest narrowed
  (`E is not io::Error` is written as the union of the remaining members).

## What the edition removes

Each is a hard error in the edition and a lint (below) in any crate before it.

| removed | instead |
|---|---|
| lossy `as` casts (truncating, sign-changing, float to int, pointer to int) | `From`, `TryFrom`, the named methods (`wrapping_`, `cast_signed`), `ptr::addr` |
| `async` and `await` | fibers and the inferred may-suspend effect |
| the ambient file, clock, network, environment, process and thread APIs | capabilities of [bstd](std.md) |
| the global allocator | an allocator the code was handed |
| global mutable state: `static mut`, a `static` with interior mutability, thread-locals | a value someone owns and passes, or a context |
| `unsafe` outside the trusted list | safe code, or a reviewed entry in the list |
| the spelling `&mut` | `&uniq` |
| derive proc macros that reflection covers | `fields_of` and friends; proc macros stay for foreign syntax |

Not touched: ownership and borrowing, traits and coherence, monomorphization, enums and `match`,
`Result` and `?`, `unsafe` as a marked boundary, and the syntax everything else already reads.

## The lints

The group `blaze::edition` is how a crate gets ready before the edition exists, and how blaze's
own crates are held to it. It is `allow` by default; a crate or workspace turns it on
(`edition = "warn"` in `clippy.toml`, or `#![warn(blaze::edition)]`). Under the edition every one
is an error. Where a mechanical fix exists, `blaze check --fix` applies it.

| lint | flags | fix |
|---|---|---|
| `blaze::lossy_as` | an `as` cast that can lose value or sign, or goes between pointer and integer | `T::from(x)` when lossless; else suggests `try_from` or the named method |
| `blaze::global_state` | a `static mut`, a `static` whose type has interior mutability (an atomic, a lock, a cell), a `thread_local!` | none: says where the state is used, so its owner can be chosen |
| `blaze::async_fn` | `async fn`, `async` blocks, `.await` | none until fibers are the runtime of the crate |
| `blaze::ambient` | reaching ambient IO (the effect of [std](std.md)) | none: names the capability to take |
| `blaze::global_alloc` | allocating from the global heap | `new_in(..)` when an allocator is in scope |
| `blaze::untrusted_unsafe` | `unsafe` outside the trusted list | none |
| `blaze::mut_ref` | `&mut` and `ref mut` where `&uniq` is available | rewrites to `&uniq` and `ref uniq` |
| `blaze::handle_clone` | `.clone()` on a `Handle` (once change 1 exists) | removes it |
| `blaze::error_wrapper` | an error enum that is only `From` variants (once change 2 exists) | rewrites to a union |
| `blaze::undeclared_effect` | a function that may panic, allocate from the global heap, block or not return where its `#[blaze::may(..)]`, its trait method's, a module's default or the gate (`#![blaze::effects]`, which the edition turns on) does not allow it ([effects](effects.md)); `deny` wherever something is declared | writes the list with the missing words: under the gate, `#[blaze::may(..)]` with exactly the inferred effects on every function that needs one |
| `blaze::unneeded_effect` | a word of a private function's list for an effect it is proved not to have | takes the word off |
| `blaze::negative_effect` | `#[blaze::nopanic]`, `#[blaze::noalloc]`, `#[blaze::nonblocking]` where effects are declared positively | rewrites to the `#[blaze::may(..)]` they stand for |
| `blaze::derivable_by_reflection` | a derive that reflection covers (once change 9 exists) | none at first |

The first three need nothing new and are built first. `lossy_as` is one lint with one message in
place of clippy's five cast lints, which stay available by their own names.

## Order of work

Payoff per unit of compiler work, and what leans on what.

| step | what | needs |
|---|---|---|
| E1 | The lint group with `lossy_as`, `global_state`, `async_fn`; turned on in blaze's own crates one at a time | nothing |
| E2 | Handles (change 1) and `handle_clone` | nothing |
| E3 | Error unions (change 2) and `error_wrapper` | nothing |
| E4 | View types (change 3) | nothing |
| E5 | Records and named arguments (change 4) | nothing |
| E5b | `&uniq` (change 10) and `mut_ref` with its fix; blaze's own crates rewritten by the fix | nothing |
| E5c | Pattern types over enum variants, with narrowing in `match` and `if let` (change 8) | nothing |
| E6 | Capabilities as parameters (change 5): blaze's crates on bstd; the `with` clause deferred | S1 to S3 of [std](std.md) |
| E7 | Effects inferred and declared at the border; closures pass their effects through; `async` removed for crates on fibers (change 6) | E6 |
| E8 | Linear values, contracts and pattern types, reflection (changes 7 to 9), each finishing its own spec | their specs |
| E9 | `edition = "blaze-2026.11"`: everything above on, everything removed an error, `blaze fix --edition` | E1 to E8, S5 |

Every change follows the rules for an extension: semantics in the front end's query crates, the
smallest general primitive, off unless asked for, used by blaze's own crates as soon as it
exists, and a test that a crate not using it builds to the same bytes.

## Why it also gives a REPL

Session bindings are handles, so a later cell copies them and ownership never strands a value.
Records give every intermediate table a type with no declaration. A cell's `with` clause is its
permission request. Effects tell the host which cells are pure and can be cached and replayed.
With live patching for redefinition, the tool and the code base are one language.

## Decided

- **`#[blaze::linear]` is the one name** for a value that must be used up; there is no second
  attribute.
- **Unions hold concrete types only** until a sound rule for parameters exists.
- **Handles need no mark at the use**; the off switch is an effect.
- **Capabilities are plain parameters.** `with` is deferred.
- **The syntax is otherwise Rust's.**
