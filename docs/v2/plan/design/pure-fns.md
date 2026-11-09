---
status: plan
summary: "#[blaze::pure] and #[blaze::pure(trusted = ..)]: functions that read the world only through their arguments, checked transitively over MIR; what counts as observing an address, which std calls are allowed, and the memory and purity facts every function gets."
date: 2026-11-02
---
# `#[blaze::pure]`: functions that read the world only through their arguments

A blaze extension (AGENTS.md rule 4: opt-in, inert under rustc with
`#![feature(register_tool)] #![register_tool(blaze)]`). A function marked `#[blaze::pure]`
promises that, given equal arguments, it returns an equal result and changes nothing but what
its arguments reach, through every call it can make. The deny-by-default lint `blaze::pure`
holds the promise ([lints.md](../../../lints.md#pure)). The kernel memoizes a compute step on
it: the step's result is kept by the identity of its instance graph and its arguments.

The check is the walk that proves build scripts pure
([pure-scripts/design.md](../../plan.md)), used a second way. It is one memoized
summary per monomorphized instance, bottom-up and closed-world. Every effect is answered from
that summary: `#[blaze::pure]` here, and `nopanic`, `noalloc` and `nonblocking` in
[effects.md](effects.md).

Code:

- `crates/hir-def/src/attrs.rs`: the attribute (`AttrFlags::PURE`, `PURE_TRUSTED`, `pure_attr`).
- `crates/hir-ty-pure/src/effects.rs`: the vocabulary (`Rule`, `EffectSet`), `PURE` and the std models.
- `crates/hir-ty-pure/src/walk.rs`: the walk.
- `crates/hir-ty-pure/src/function.rs`: the check, the refusal and the proof.
- `crates/hir-ty-pure/src/memory.rs`: the memory facts.
- `crates/blaze-lints/src/blaze/effects.rs`: the lints, all four.
- Tests: `crates/hir-ty-pure/src/tests/fns.rs`.

## Interface

```rust
// hir-ty-pure
/// `func<args>`, monomorphized, with `krate`'s impls in scope. A query.
pub fn fn_purity(db: &Db, krate: Crate, func: FunctionId, args: StoredGenericArgs) -> &FnPurity;
/// The definition in its own where-clauses: a call only an instance decides is `open`.
pub fn fn_purity_generic(db: &Db, func: FunctionId) -> &FnPurity;
/// The same for any effect in `EFFECTS` (`fn_purity` is `check(.., &PURE)`).
pub fn check(db, krate, func, args, effect: &EffectDef) -> Result<EffectProof, Refusal>;
pub fn check_generic(db, func, effect: &EffectDef) -> Result<EffectProof, Refusal>;

pub enum FnPurity { Pure(EffectProof), Impure(Refusal) }
pub struct EffectProof { pub trusted: Box<[Trusted]>, pub open: bool, pub instances: u32 }
pub struct Trusted { pub func, pub path, pub reason: Symbol, pub krate, pub link: Option<Symbol>, pub door: Option<TraitId> }
pub struct Refusal { pub effect, pub function, pub rule: Rule, pub says, pub what, pub chain: Box<[Step]>, pub fix }
pub struct Step { pub name: Box<str>, pub at: Option<Place> }      // Place: file, path, line, column
impl Refusal { pub fn said(&self) -> String; }

/// Inferred for any instance, attribute or not.
pub fn fn_memory(db, krate, body: Body, args) -> MemoryEffects;   // level 1
pub fn instance_effects(db, krate, body: Body, args) -> EffectSet; // every kind, unmasked
```

A refusal names the pure function, the shortest chain of calls to the operation, each step's
file and line, and the rule:

> `kernel::compute` is `#[blaze::pure]`, but reads the environment: `kernel::compute`
> (src/lib.rs:17) → `kernel::middle` (src/lib.rs:13) → `kernel::helper` (src/lib.rs:9) →
> `kernel::deeper` (src/lib.rs:2), which calls `std::env::var`; a pure function reads the
> outside world only through its arguments

## The attribute

`#[blaze::pure]` goes on a function, a method, a foreign function or a trait. It takes one
optional argument:

- A bare word: checked. On a trait, every method of every impl of it is checked as if marked,
  so "every compute step is pure" is a lint on `trait ComputeStep`.
- `(trusted = "why")`: believed, not checked. Its body is not walked for purity, but the
  closures and function items it is given (its type arguments) are, with their drop glue. Its
  reason is recorded in `EffectProof::trusted`, sorted by path, so a reviewer reads every
  promise made by hand that a proof rests on. On a foreign function the entry carries the
  symbol it links (`link`), and the kernel adds that native library's hash to the entry's
  identity. On a trait (the `ctx` door, `trusted = "the kernel records every read"`), every
  impl's method of it, and every call through `dyn` of it, is trusted (`door: Some(trait)`).
- Anything else: the lint refuses it, naming the text.

No codegen key reads `AttrFlags::PURE` or `PURE_TRUSTED`: a function without the attribute
builds byte for byte as before. The attribute changes only what the lint says.

## What a pure function may do

Allowed:

- **Allocate and free.** It may use the global allocator, any `Allocator` (`Vec::new_in(frame)`)
  or a `GlobalAlloc` impl. The walk stops at the allocator and records `Alloc` or `AllocIn`,
  which purity does not refuse. A fresh allocation's address differs per run, so it is allowed
  only while nothing observes it (below).
- **Panic or overflow.** The job fails and nothing is kept. `Panic` and `Overflow` are recorded,
  not refused.
- **Diverge.** Under a callee returning `!` (`panic!`, `abort`, `process::exit`), no kind counts
  for purity (`EffectDef::through_never = false`): it never returns a result to keep. The other
  effects count them.
- **Mutate memory it owns or was given `&mut`**, including `Cell`, `RefCell` and atomics on that
  memory. It may read through raw pointers inside toolchain types (`Vec`, `Box`, `NonNull` in
  `Rc`).
- **Format, hash and sort values**: `format!`, `to_string`, `Display`, `Hash` with a fixed
  seed, `sort`, `BTreeMap`, `blaze_hash::HashMap`. The walk is closed-world: a function
  pointer or vtable (a `fmt::Argument`'s formatter, a `dyn Write`) is walked where it is made.
- **Call foreign functions known to be deterministic** (`effects::deterministic_foreign`): the
  allocator's shims (`__rust_*`, `__rdl_*`, `__rg_*`), `memcpy memmove memset memcmp bcmp
  strlen`, libm (and its `f` forms), and LLVM intrinsics other than those that read or set the
  machine's state (`rdtsc`, `rdrand`, the FP environment, `read_register`, and so on).
- **Call trusted functions.** `blaze_graph::par`'s ordered combinators (`map`, `filter_map`,
  `join`, `extend`, `for_each`) are meant to carry
  `#[blaze::pure(trusted = "results come back in item order")]`: their threads are not walked,
  but the closure given to them is. `par::threads` and `par::scope` stay unmarked. The marks
  themselves land with blaze-graph (see "Left").

Refused, by `Rule` (`PURE_REFUSES`):

| rule | what | how it is found |
|---|---|---|
| `Io` | files, the network, stdio (`println!`, `dbg!`), processes | models' leaves (`Leaf::Print`, `ReadFile`, ...) |
| `Env` | `std::env::{var, vars, args, current_dir, current_exe, home_dir, temp_dir, set_var, remove_var, set_current_dir}`, `process::id`, `thread::available_parallelism` | `MODELS` rows and `Leaf::EnvVar` |
| `Clock` | `Instant::now`, `SystemTime::now` | `MODELS` rows |
| `Thread` | `thread::{spawn, scope, sleep, park, yield_now, current, panicking}`, `Builder::spawn`, `JoinHandle::join` | `MODELS` rows |
| `MutableState` | `static mut`, a static that is not `Freeze`, a thread-local | the walk, at the use |
| `Address` | observing an address (next section) | the walk and `MODELS` rows |
| `Random` | `RandomState::new` and its `Default`, so std's `HashMap::new()` | `MODELS` rows |
| `Foreign` | a foreign function or static not trusted and not in the deterministic list, inline assembly, volatile memory, the machine-state intrinsics | the walk |
| `Unchecked` | what the walk can't follow: a body that does not lower, an `async` closure, a `dyn*`, a code pointer made of bytes, an instance past `LONGEST` | the walk |

`std::sync` locks, `Condvar` and channels end in a futex or `pthread` call and are refused as
`Foreign`. A lock the function owns has no other thread to wait for: use `&mut` or a `RefCell`.

The root's own parameters are scanned too, because a pure function may only read the world
through them:

- A shared reference to a type that is not `Freeze` (`&AtomicU64`, `&Mutex<T>`, `&Rc<T>`) is
  `MutableState`: another thread or a clone can change it during the call.
- These are `Unchecked`: a raw pointer outside a toolchain ADT, a function pointer, a `dyn`
  that is not of a trusted trait, and a union of a non-toolchain crate. Take a type parameter
  instead, so that each instance is checked.
- A type parameter left in a generic check is `open`. Each instance decides, and the
  monomorphized check refuses it as `Unchecked` if it is still unknown.

## What counts as observing an address

An address is the allocator's and the code generator's choice, so a result that depends on one
is not a function of the arguments. These count as `Address`:

- Casting a number to a pointer (`PointerFromExposedAddress`, `with_exposed_provenance`,
  `simd_with_exposed_provenance`), in every crate. It is an address made from a number.
- Formatting a pointer (`{:p}`, `core::fmt::pointer_fmt_inner`), hashing a raw pointer
  (`<*const T as Hash>`), and ordering raw pointers through `PartialOrd` and `Ord`. These are
  counted from every crate.
- In a workspace crate's body only:
  - casting a pointer to a number (`as usize`, `PointerExposeAddress`) or transmuting one into
    a number;
  - `addr`, `expose_provenance`, `align_offset`, `is_aligned` and `is_aligned_to` of a raw
    pointer or `NonNull`, and `<[T]>::align_to(_mut)`;
  - ordering raw pointers with the built-in `<`, `<=`, `>` or `>=`;
  - comparing function pointers at all, since whether two are equal is the code generator's
    choice.

  The toolchain's own uses stay inside one allocation (a slice iterator's `end`, a map's bucket
  offset), so they are not counted there.

Not counted: equality of raw pointers (`ptr::eq`, `Arc::ptr_eq`). Two live allocations never
overlap, so equality says only whether they are the same one. The known gap is a dangling
pointer compared with a fresh allocation, which may or may not be equal. A comparison of
offsets within one allocation (`offset_from`) is not counted either.

## Inferred facts, for every function

Both facts come from the same walk and are computed for any instance, with or without the
attribute. The attribute only makes the lint refuse a failing level 2.

- **Level 1, memory effects** (`fn_memory`). This is what common-subexpression elimination
  (CSE) and loop hoisting need, like LLVM's `memory(..)`, `nounwind` and `willreturn`, but
  whole-program. It covers:
  - `reads`, `writes` and `returns`, each on the scale `None < Direct < Args < Any`. `Direct`
    is LLVM's `argmem`, one dereference deep. `Args` is anything reachable from the arguments.
  - `allocates`, `unwinds` and `diverges`.

  The walk computes this from its own provenance lattice. Trust marks are not believed here, and
  a body that does not settle within `4 * (locals + 2)` rounds is `UNKNOWN`.
- **Level 2, full purity** (`fn_purity`), the check above.

Both are deterministic: blocks are walked in order, the lattice is finite, the fixpoint is
order-independent, and trusted lists are sorted by path. Both are keyed by every input: the
instance, its arguments, and the crate whose impls are in scope.

Rule 18 applies to any pass that changes a caller's code because of these facts (merges two
calls, hoists one, drops an unused one). That pass puts what it used into the caller's code
key: `(callee instance identity, MemoryEffects::bits())` for level 1, or
`(callee instance identity, 1)` for level 2. A change three calls down that turns a read-only
callee into a writing one changes the bits, and with them every caller optimised on the old
ones.

## The effects seam

There is one walk, with one summary per instance, and it answers every effect.

- Each summary carries `kinds`, every `Rule` the instance or its callees may have, unmasked.
- Each effect `e` in `EFFECTS` also gets a set: the kinds `e` refuses, plus `Open`, after
  removing what `e` trusts. Each kind keeps its shortest witness: the operation with its span,
  or the call site it comes through.
- An attribute is an `EffectDef`: a name, a `refuses` mask, the attribute reader, how it phrases
  each rule, its fix and `through_never`. Checking one reads the root's summary and never walks
  again.
- Trust is per effect: a function or trait trusted for `pure` is still walked for `nopanic`.

The three in [effects.md](effects.md) (nopanic, noalloc, nonblocking) add their kinds (`Block`,
`AllocIn`, `ForeignUnknown`, `ForeignUnwind`) and rows on this seam. `#[blaze::constant_time]`
is dataflow, not reachability: it can share `EffectDef` for its attribute and lint, but needs a
pass of its own over each body.

## `#[blaze::cache]` on this check

There is one purity check. fncache's `#[blaze::cache]` (crates/hir-ty-codegen/src/fncache.rs)
asks `fn_purity` plus its own argument rule (what a key can serialize), and keeps only the
cache. Each fncache rule now lands in this check:

- Its foreign allow-list becomes `deterministic_foreign`.
- Changing statics and thread-locals become `MutableState`.
- asm and foreign statics become `Foreign`.
- Its `gives_address` checks become the `Address` rules above. This check is stricter: it also
  catches a pointer-to-number transmute.
- Its `-> !` leaves become `through_never`.

Still to port:

- The `RandomState` exemption inside std's `HashMap`/`HashSet`, and the in-order rows on those
  maps (`iter`, `keys`, `drain`, ...) conditioned on the hasher argument.
- `Rule::Count` for `Rc`/`Arc`/`Weak` counts, refused only when a root argument holds one.
- fncache.rs calling `check(.., &PURE)` in place of its own walk.

## Left

- Mark `blaze_graph::par`'s ordered combinators and `blaze_mem::frame::current` trusted, with
  `register_tool(blaze)` in those crates.
- Fold `hir-ty-mir`'s const-purity check (`mir/purity.rs`, lint `blaze::const_impure`) onto
  this one.
- Close the dangling-pointer equality gap if a real case appears. Provenance in `memory.rs`
  could say "the same allocation".
