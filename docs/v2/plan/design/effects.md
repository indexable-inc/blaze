---
status: plan
summary: "#[blaze::expect(..)]: a function says exactly what it does (panic, alloc, block, diverge); what it leaves out is checked absent and what its own list has is checked present, over MIR on pure's walk. The old spellings (nopanic, noalloc, nonblocking), which loops are known to end, and where #[blaze::constant_time] would sit."
date: 2026-11-12
---
# Effects: what a function is expected to do

One blaze extension (AGENTS.md rule 4: opt-in, inert under rustc with
`#![feature(register_tool)] #![register_tool(blaze)]`, changing no code that blaze builds) says
what a function, and everything it can call, is expected to do:

```rust
#[blaze::expect(panic, alloc)]
pub fn parse(text: &str) -> Tree { .. }
```

| word | the function may | the old spelling of its absence |
|---|---|---|
| `panic` | panic or abort | `#[blaze::nopanic]` |
| `alloc` | call the global allocator (an allocator it is handed is a capability, not this) | `#[blaze::noalloc]` |
| `block` | block the thread | `#[blaze::nonblocking]` |
| `diverge` | not return: a loop not known to end, or recursion | none |

A word the list leaves out is an effect the function does not have, checked through every call
it makes; a word a function's own list has is an effect it has, as Rust's own `#[expect(lint)]`
is fulfilled or reported. `#[blaze::expect()]` (or `#[blaze::expect]`) is a function with none
of the four. The name was `#[blaze::may(..)]`, a permission; it is deleted, with no alias: written,
it is refused (`blaze::undeclared_effect`) naming `blaze::expect`, and the fix respells it.

**Why the list is positive.** A signature that lists what can happen is complete to its reader:
nothing unlisted happens, so there is no second question ("and does it block?") the mark leaves
open, as `#[blaze::nopanic]` alone does. A new effect touches only the functions that have it,
where a negative spelling would have to be added to every function that does not. It matches
capabilities, which are already positive (a function touches the disk because it was handed a
`Dir`). And a list that is exact is never stale: listing too little and listing too much are
both errors, so a reader can take every word as true.

There is no word for code the check can't see (a foreign function no table knows, inline
assembly, a function pointer taken as a parameter, a call of an unlisted method through a
`dyn` taken as a parameter, a body that does not lower): it may do anything, so it is refused for every word the list leaves out. Only
`#[blaze::expect(panic, alloc, block, diverge)]`, "anything", admits it, or a callee taken at
its word (`trusted`, below). `pure` needs no word either: see "Purity" below.

They are checked on the one walk that `#[blaze::pure]` uses ([pure-fns.md](pure-fns.md)): one
memoized summary per monomorphized instance, carrying an `EffectSet` (one bit per kind of
effect, `hir_ty_pure::effects::Rule`) with a witness of each. Each word has an `EffectDef`
(`effect_attrs::of_word`: `NOPANIC`, `NOALLOC`, `NONBLOCKING`, `TERMINATES`), a mask over those
kinds; a list without the word asks for it, and checking one reads the root's summary.

Code: `crates/hir-def/src/attrs.rs` (the attributes: `ExpectWord`, `ExpectAttr`,
`AttrFlags::EXPECT` bit 101, `BLAZE_EFFECTS` bit 102; the old spellings' `EffectWord`, bits 97
to 99), `crates/hir-ty-pure/src/expect.rs` (the one home of what is declared: `declared`,
`Marks` with `exact`, `allowed`, `unfulfilled`, `verdict`), `effect_attrs.rs` (the
definitions, the site rules and the tables), `loops.rs` (which loops end),
`crates/blaze-lints/src/blaze/expect.rs` (the lints of the list and of the deleted spelling)
and `effects.rs` (the old spellings' lints).

## What is declared, and where it comes from

A function's *allowance* is the set of words it may have. The first of these that is written
holds, alone; nothing is merged (`hir_ty_pure::expect::allowed`):

1. **Its own list**, `#[blaze::expect(..)]` on the function. What is written on the function is
   what holds: it replaces a default around it entirely, so `#[blaze::expect(alloc)]` in a module
   whose default is `panic` may allocate and may not panic.
2. **Its trait's**, for a method of an impl of a trait (or a trait's default method): the
   `#[blaze::expect(..)]` of the trait's method, else of the trait. A trait method's list is the
   bound every impl must stay within. An impl's method may write its own list with fewer words,
   never more: one with a word the trait's method lacks is refused.
3. **The default around it**: `#![blaze::expect(panic)]` in a module or at the crate's root is the
   allowance of every function in it with none of the above; the nearest module's holds.
   `#![blaze::effects]` at the crate's root is the default "nothing": every function of the
   crate with no list on it or around it may have no effect. This is the gate. The blaze
   edition ([edition.md](edition.md)) will turn it on; `edition = "blaze-2026.11"` is not on
   main yet, so the gate is this attribute today.
4. **None.** A function none of these reach is of code that asked for nothing: it may do
   anything, and it is not checked. To its callers it is what the walk infers of its body, as
   before: a marked function that calls into an unmarked crate is held to what that crate's
   code does, not to "anything". So a crate on an ordinary edition is linted, and built, exactly
   as before (rule `superset-of-rust`).

**Under-declaring is an error** (`blaze::undeclared_effect`, `deny`): a function with an effect
outside its allowance is refused at its name, saying each such effect with its witness (the
call chain down to what has it, each step with its file and line, or the loop's line), where
the allowance is written, and the fix: "add `panic` to its `#[blaze::expect(..)]`, or handle it".
`blaze check --fix` writes the list with the missing words added (a new list above the
signature where the function had none), which is how a crate is brought under the gate: turn
`#![blaze::effects]` on and apply the fixes. It replaces the four `unmarked_*` lints of the
earlier design.

**Over-declaring is an error too** (`blaze::unfulfilled_effect`, `deny`, with a fix that takes
the word off): a function's own list is exact, so a word of it for an effect the function is
proved not to have is an unfulfilled expectation. It applies to every function with a body
whose list is its own, private and public alike, and to an impl's method that writes its own
list. The finding names the function, the word and why it is known not to hold ("no call or
operation in it allocates from the global heap, through every call it makes"); `panic` is
proved absent only where the crate keeps overflow checks, so the shorter list holds in dev and
in release. The cost is plain: a callee that stops having an effect makes each caller's word
for it unfulfilled, so removing an effect reaches the callers; their fix is automatic
(`blaze check --fix` takes the word off, caller by caller up the chain).

Where exactness has nothing to prove, the list stays a bound:

- A trait method's list bounds its impls (a default body among them): it says what any impl
  may do, not what one does.
- A list a function gets from its trait or from a module's default and did not write is not
  checked for unfulfilled words: a default cannot be exact for every function under it. An
  impl's method that inherits its trait's list is held within it; one that writes its own is
  held exact.
- A `trusted` list is taken at its word.
- Where the function's effects are not known (a `dyn` call with no list, a body that does not
  lower), a word is neither proved nor refuted, so it is not reported.

`blaze::unneeded_effect`, the advice this was (a private function's word only), is deleted, not
renamed: naming it is reported as a removed lint.

**`trusted`.** `#[blaze::expect(alloc, trusted = "why")]` is a list taken at its word: the function
is not checked, and every call of it has exactly the listed effects (on a trait: every impl's
method, and every call through `dyn` of it). The reason is recorded in the proof
(`EffectProof::trusted`) so a reviewer reads every promise made by hand. A module's default
takes no `trusted`.

**Calls the check can't resolve.**
- *Through `dyn Trait`, or a method of a type parameter*: the call has the trait method's list
  (its own, else its trait's), and a generic function's call of such a method is not left open
  to each instance. This rests on every impl being held within the list, which
  `undeclared_effect` does wherever the impl is written.
- *A `dyn` taken as a parameter* (itself, in a field, behind a pointer) is an object the
  caller made, which the walk of the function never saw. Taking it is not refused: a call
  through it is, when nothing says what the object's method may do. The walk records each call
  through a `dyn` of a method whose list (or whose trait's trust) does not leave the word out,
  each call through a `dyn Fn`, and each drop of a value that owns a `dyn` (`Rule::Dyn`, kept
  for every word); a function that takes a `dyn` and reaches one, in its own body or in
  anything it calls, is refused as code the check can't see, naming the method and the `dyn`.
  A callee `trusted` for the word does not hide it: it answers for its own code, not for the
  object it is handed. So a function that stores the `dyn`, compares it or hands it on to code
  that calls nothing through it is clean, and one that calls a listed method has that list.
  `PhantomData<T>` in a parameter's type is looked into for what a `T` owns (a `Vec`'s
  elements are named only by its `PhantomData<T>`), except a function pointer type:
  `PhantomData<fn() -> T>` holds no pointer and no `T`.
- *Through a function pointer*: a function pointer's type has no marks, so one taken as a
  parameter is "may anything" and refused for every word the function leaves out. One made in
  the body is different: what it points to was walked where it was made (the walk is
  closed-world), so its panics, allocations and waits are already counted, and the call adds
  only `diverge`, since the pointer may lead back to the caller. Effects in function pointer
  types (`fn(u32) -> u32 expect(panic)`) would lift both: the type would carry the list, a
  coercion of a function to it would check the function's list against it, and a call through
  it would have the type's list. That needs the list in the type system (a part of `FnSig`,
  with subtyping from fewer words to more), not an attribute.

## The old spellings

`#[blaze::nopanic]`, `#[blaze::noalloc]` and `#[blaze::nonblocking]` (each with the optional
`(trusted = "why")`, on a function, a foreign function or a trait) stay accepted in every crate
and mean exactly what they meant: a promise of the absence of one effect, held by their own
lints (`blaze::nopanic`, `blaze::noalloc`, `blaze::nonblocking`). Internally there is one
representation, the allowance: an old spelling lowers to "every word but this one"
(`expect::declared`), so a function with one is not held to a default around it for the others.
Where effects are declared (under a module's default or the gate), `blaze::negative_effect`
(`warn`) asks for the positive form, and its fix writes it: every word the old spellings do not
take off that the function is not proved free of. A function with `nopanic` and `nonblocking`
that allocates and has a loop not known to end becomes `#[blaze::expect(alloc, diverge)]`. A
function with both spellings is refused. `diverge` has no negative spelling: there never was a
`#[blaze::terminates]`.

`AttrFlags::effect_attr(db, owner, EffectWord)` reads an old spelling's argument (one bit test
for an item without one); `AttrFlags::expect_attr(db, owner)` reads a list (`ExpectAttr`: the
words, `trusted`, or the text of one that is malformed). The deleted `#[blaze::may(..)]` sets
no bit and is read by nothing but the lint that refuses it. No codegen key or signature reads the bits:
a function without an attribute builds byte for byte as before, and so does one with it.

## Purity

`#[blaze::pure]` is not a word of the list and keeps working as it does
([pure-fns.md](pure-fns.md)): it is what a cache of a function's results keys on. It is
derivable: `hir_ty_pure::derived_pure(db, func)` is the attribute's own check (the walk, and
the scan of the parameters for state behind shared references and code the check can't see)
asked of a function nobody marked, true where it passes with no call left to an instance. So a
function with no effect of the world and the purity conditions is pure whether or not the word
is written, and what keys on the attribute may key on the derived answer where it holds. This
slice adds the query; no consumer is switched to it yet, and the attribute stays the way to
make purity a promise the lint holds.

## `diverge`: what "returns" means, and which loops end

A function that may not `diverge` **returns or panics; it never runs for ever**. A panic or an
abort ends it, so `diverge` and `panic` are independent: `#[blaze::expect(panic)]` is "ends, maybe
by panicking", and `#[blaze::expect()]` is total: it returns. A wait is not its business either:
the function returns whenever every wait it makes does, and leaving out `block` says there is
none.

What the walk records (`Rule`), all refused by a list without `diverge`:

- `Loop`: a loop of the body not known to end, at the loop's line.
- `Recursion`: the instance can call itself again, through any calls. The refusal names the
  chain and says which function is called again.
- `Indirect`: a call through a function pointer or a `dyn`, or the drop of a value that may
  hold a `dyn`. What it reaches was walked where the pointer or vtable was made, but it may
  reach the caller again, a recursion no cycle of instances shows. A `dyn` call of a trait
  method whose list leaves `diverge` out is not one.
- and, as for every word, what the check can't see (`ForeignUnknown`, `Unchecked`).

**Loops** (`crates/hir-ty-pure/src/loops.rs`). A loop is a strongly connected component of the
body's control flow graph; a block that goes to itself counts. A component ends when some block
of it, a *bounding block*, can run only finitely often while control stays in the component,
and what is left without that block ends too. The bounding rules are data for that one
algorithm (`loops::Bounding`). This slice has one, sound from the MIR alone:

- **A counted `while`.** `while i < n { ..; i += c; }` (and `<=`, and the mirror with `>`,
  `>=` and `-=`), where `i` is a local integer, the limit is a constant or a local the loop
  does not write, every write of `i` in the loop is a step by a positive constant the same
  way, every path round the loop passes a step, and no local involved has its address taken.
  **Overflow is accounted for by the profile**, as `nopanic` is: where the crate keeps overflow
  checks (a dev profile, or `overflow-checks = true`) a step is `AddWithOverflow`, an `Assert`
  and the sum, and a wrap is a panic, which ends the function, so any positive step and `<=`
  are bounds. Where the crate drops them (release), a step wraps, so it is a bound only when
  it cannot: the test is strict, the step is by one, and it is the loop's only write of `i`
  and runs at most once a turn (`i < n` then gives `i + 1 <= n`). `while i <= n { i += 1 }`
  and `while i < n { i += 3 }` therefore pass in dev and are refused in release, where `i` can
  wrap round for ever; the same function can need `diverge` in one profile and not the other.

**Everything else is refused**, naming the loop's line, until a rule for it exists:

- `for`. A `for` over a range or a slice ends (the iterator's `next` is finite and its `None`
  edge leaves the loop), and that rule is the next one; it is not in this slice because the
  test toolchain's `Range::next` is `loop {}`, so it could not be proved by a test here, and an
  untested rule is not shipped.
- `while i < v.len()` (the limit is computed in the loop), `while let Some(x) = stack.pop()`,
  `loop { .. break .. }`, iterator adaptors, and every loop inside std that these rules do not
  cover. A callee proven by hand is marked `#[blaze::expect(.., trusted = "why")]`.
- A **`decreases` clause** proved by the verifier lane ([contracts.md](contracts.md)) is the
  general answer: a measure that falls on every turn. It is one more `Bounding`, tried on a
  component after these; the algorithm and its test do not change.

**Recursion and the fixpoint.** A cycle of instances is a cycle of the summary query, which
starts from `Summary::recursive()`: the summary whose only kind is `Recursion`. Every member of
the cycle reads it, of itself or through the others, so at the fixpoint each member and each
caller has `Recursion` through a call site. Every other kind's witness has a depth (the fewest
calls to where it is done) that grows by one per call; around a cycle that would grow each
round and the fixpoint would never settle, so `Recursion`'s witness has one constant depth and
the first site that has it stays. The refusal follows the witnesses and stops where the next
instance is one already in the chain (and after 64 steps at most), saying it is called again.

## What each old attribute refuses

The kinds (`Rule`) the walk gives each site, and which effect refuses which (`diverge`'s
kinds are above):

| kind | where the walk finds it | nopanic | noalloc | nonblocking |
|---|---|---|---|---|
| `Panic` | an `Assert` the MIR keeps (bounds, division, a UB check), the panic and abort entry points (`core::panicking::*`, `unwrap_failed`, `std::process::abort`, `handle_alloc_error`, ...), the `abort` intrinsic, the panic runtime's foreign symbols | yes | | |
| `Overflow` | an optional overflow `Assert` of a body that inherits the setting (`#[rustc_inherit_overflow_checks]`); one its own crate keeps is `Panic` | when the marked crate checks overflow | | |
| `Alloc` | `__rust_alloc*`, `alloc::alloc::{alloc, alloc_zeroed, dealloc, realloc}`, a `GlobalAlloc` impl's methods, `Global`'s and `System`'s `Allocator` methods, `malloc` and its family, `box_new` | | yes | |
| `AllocIn` | any other `Allocator` impl's method (`Vec::new_in(frame)`) | | | |
| `Block` | the blocking table (below) | | | yes |
| `ForeignUnwind` | a foreign function blaze knows nothing of whose ABI may unwind and that is not `#[rustc_nounwind]`; `asm!(.., options(may_unwind))` | yes | | |
| `ForeignUnknown` | a foreign function not in the foreign table; inline assembly | | yes | yes |
| `Unchecked` | what the walk can't follow (a function pointer from a parameter, a call through a `dyn` from a parameter that no list answers for, a body that does not lower) | yes | yes | yes |

A list without `diverge` refuses `Loop`, `Recursion`, `Indirect`, `ForeignUnknown` and
`Unchecked`.

A refusal says the shortest call chain from the marked function to the site, each step with its
file and line, the rule, and what the last step does:
"`sched::Place::pick` is `#[blaze::nonblocking]`, but may block the thread: `sched::Place::pick`
(src/sched/mod.rs:860) → `Place::steal_for` (src/sched/mod.rs:915) → ..., which calls
`std::sync::Mutex::lock`, which waits for the lock". Then the fix: wait through the scheduler,
or mark the callee `trusted` with why.

What is under a panic entry point is not counted for any effect: the panic machinery (the hook's
lock, the payload's box) runs only while the job is failing (rule 17), and `nopanic` refuses the
call itself. Likewise the allocator's own body is not walked: `noalloc` refuses the call, and
`nonblocking` does not count the locks inside an allocator (the global allocator is treated as
non-blocking; code that must not even take malloc's lock is `noalloc`).

### Which MIR, which profile

The walk reads the monomorphized runtime MIR (`monomorphized_runtime_mir_body`: after drop
elaboration, before optimization). Its optional overflow `Assert`s are built per the crate the
body is written in (`transform::asserts`): a body of a crate built with `overflow-checks` has
them and they run, so they are `Rule::Panic` like a bounds check; a body of a crate built
without (std, a release profile) has none. The one case the body's crate doesn't decide is a
body that inherits the setting from the crate it is compiled in
(`#[rustc_inherit_overflow_checks]`: `u32::pow`, `<u32 as Add>::add` called from generic code):
its checks are `Rule::Overflow`, and `nopanic` takes the profile of the marked function's crate
(`hir_ty_mir::mir::overflow::crate_checks_overflow`) for them: with overflow checks (a dev
profile, or `overflow-checks = true`) it is `NOPANIC` and they are refused; without (release) it
is `NOPANIC_NO_OVERFLOW_CHECKS` and they pass. A lang crate's setting is `None` and is never
guessed: a marked function in one is a finding saying so. The same marked function can pass in
release and be refused in dev; the lint says which profile it checked. Known approximation: an
inherited body reached through a dependency whose `[profile.*.package]` overflow setting differs
from the marked crate's is judged by the marked crate's setting (cargo's default profiles set it
for every crate alike).

Constant-folded asserts are fine: the runtime MIR is not folded, so the check folds the one shape
that matters itself (`assert_rule`): an `Assert` whose condition is a constant, or a local the
block last assigns a comparison of two constant unsigned integers (what `arr[2]` on a `[T; 4]`
lowers to), is dropped when the constant holds. Any other `Assert` stays a refusal naming its
line.

No unwinding call into code not proven nopanic: every Rust callee is walked, so its panics are
its own refusal; a foreign callee is `Panic` or clean by the table, and one the table does not
know is `ForeignUnwind` when its ABI may unwind (`"Rust"`, `"C-unwind"`, `"system-unwind"`, ...).

### How `noalloc` tells an explicit allocator from the global one

By type, in MIR, at the call: a call that dispatches to a method of an `Allocator` impl is
`AllocIn` unless the impl's `Self`, the instance's arguments substituted and references taken
off, is `alloc::alloc::Global` or `std::alloc::System` (the global heap), which is `Alloc`; every
method of a `GlobalAlloc` impl is `Alloc`. So `Vec::new_in(frame).push(x)` reaches
`<&Frame as Allocator>::allocate`, `AllocIn`, which `noalloc` allows: AGENTS.md rule 9's
`Vec::new_in(frame)` / `Box::new_in(.., frame)` is exactly what a noalloc function may do. And
`Vec::new().push(x)` reaches `<Global as Allocator>::allocate`, `Alloc`, refused. The allocator
impl's own body is not walked (where a frame gets its next chunk is its business, not the
caller's). No data flow is needed: `Global` is a type argument of every collection that uses it.

### The blocking set, as data

`effect_attrs::ROWS` (functions by path, resolved once per crate through its def map) and
`effect_attrs::FOREIGN` (foreign functions by link symbol):

- std: `thread::{sleep, sleep_ms, sleep_until, park, park_timeout, park_timeout_ms}`,
  `JoinHandle::join`, `sync::{Mutex::lock, RwLock::{read, write}, Condvar::wait*, Barrier::wait,
  Once::{call_once, call_once_force, wait, wait_force}, OnceLock::{get_or_init, get_or_try_init,
  wait}, LazyLock::force}`, `mpsc::{Receiver::recv*, SyncSender::send}`,
  `process::{Child::wait*, Command::{output, status, spawn}}`,
  `net::{TcpStream::connect*, TcpListener::accept}`, `io::Stdin::{read_line, lock}`.
- registry crates, by crates.io name and only with a registry origin (a workspace crate named
  `lock_api` is not taken for it): `parking_lot_core::park`, `lock_api::{Mutex::lock,
  ReentrantMutex::lock, RwLock::{read, write, upgradable_read, read_recursive}}`,
  `parking_lot::{Condvar::wait*, Once::call_once*}`, `crossbeam_channel::{Receiver::recv*,
  Sender::send*, Select::{select, ready, ..}}`, `crossbeam_utils::sync::{Parker::park*,
  WaitGroup::wait}`.
- foreign: the blocking syscalls and libc calls (`read`/`write` and their vector and positional
  forms, `recv*`/`send*`, `accept`, `connect`, `poll`/`ppoll`/`select`/`epoll_wait`/`kevent`,
  `nanosleep` and the other sleeps, `wait*`, `syscall` (std's futex wait on Linux), `__ulock_wait`,
  `os_sync_wait_on_address*`, the `pthread_*` waits and locks, `sem_wait`, `open*`, `close`,
  `fsync`, `flock`, `fcntl`, `getrandom`, `getaddrinfo`, and the Windows waits, sleeps, SRW
  lock acquires and file and socket IO); the allocator's and the panic runtime's symbols; and the
  clean ones (memory, the clock, ids, thread-local keys, wakes, yields, page mapping), which only
  `pure` refuses.

`read` and `write` on a descriptor are blocking whatever the descriptor: the check can't prove an
`fd` is `O_NONBLOCK`. A wrapper that knows it says so: `#[blaze::nonblocking(trusted = "the fd
is O_NONBLOCK, set in Poller::new")]`. A `try_lock`, `try_recv` or `try_send` is not in the
table and is walked like any function: its body does not wait.

`check_rows_resolved(db, krate)` names every row of a crate's table that resolves to nothing; a
test over the pinned std, core and alloc holds the table to the toolchain, as `models` does.

## Where blaze uses them

Marked with the old spellings, before the list existed, after reading the code (each holds by
the rules above; the lints hold them from here on; `negative_effect` rewrites them when their
crates come under the gate):

- `blaze_graph::sched::Place::{pick, take_own, steal_for, steal_half, nearest_parked,
  search_parked}`: `#[blaze::nonblocking]`. A free core's pick and steal, the path every pool
  thread takes between two jobs; the scheduler is a sans-IO core (rule 12) and must never wait.
  They allocate (their queues grow), so not `noalloc`.
- `blaze_graph::sched::cores::first_from`: `#[blaze::noalloc]`, `#[blaze::nonblocking]`. The
  bitmap scan under every steal and wake.
- `hir_def::nameres::facts::graph::ImportGraph::from_edges`: `#[blaze::noalloc]`. The import
  graph of every expansion round, built in the query's frame.

Not marked: `pool.rs`'s injector takes a `parking_lot` lock (`Inject::push`, `pop`,
`steal_into`), so a worker's search is not `nonblocking` and is not marked; the query loops
that use a frame also call the database, which allocates its memos from the global heap.

## Tests

Per effect, one crate that passes and one per kind of refusal, named for the property
(`crates/blaze-lints/src/blaze/effects_tests.rs` for the old spellings, `expect_tests.rs` for
the list: each word's under-declaration with its witness, the counted `while` and its near
misses, recursion, function pointers and `dyn`, an impl within its trait's list, a module's
default and a function's own list, an unfulfilled word refused on a private and a public
function with its fix (control: the old private-only rule lets the public one through), the
bounds that pass with an unneeded word (a trait method's, an inherited one, a default, a
`trusted` one), an impl method's own list held exact, the deleted `#[blaze::may(..)]` refused
and respelled, the rewrite of the old spellings, and a crate that declares nothing), plus `an_effect_attribute_changes_no_signature` in hir-def: a
function's signature and flags other than the effect bits are the same with the attributes
and without, so an unmarked crate builds byte for byte the same.

Two invariants are stated and tested with Hegel, each with planted-bug controls:

- `loops.rs`: a control flow graph the analysis accepts has no infinite path. Random small
  graphs with random bounding facts, model-checked by brute force. Controls: a self-loop not
  seen as a cycle; a bounding block taken off a component its exit stays in.
- `expect.rs`: the checker accepts exactly when every function's inferred effects are within
  its allowed set, where allowed sets flow as specified, and every word of an own list held
  exact is an effect the function has. Random call graphs, effect sources and declarations
  against a reference model. Controls: an impl allowed more than its trait; a module's default
  merged with a function's own list; an own list treated as a bound.

## `#[blaze::constant_time]` on the same seam (later, not this lane)

`constant_time` promises that the time a function takes, and the memory it touches, do not depend
on its secret inputs: no branch, index or variable-time instruction on a secret. It is dataflow,
not a call-graph effect: whether a `SwitchInt` or an index is refused depends on whether its
operand is tainted by a secret, which this walk does not track. How it sits on the seam:

- The attribute and its `trusted` door are the same: a bit in `AttrFlags`, `effect_attr`, an
  `EffectDef` in `EFFECTS`, a lint by the same `EFFECTS`-generic loop, and the same `Refusal`
  with its chain and `said()`. Secrets are named by a second attribute on parameters and fields
  (`#[blaze::secret]`).
- The kinds are new `Rule`s (`SecretBranch`, `SecretIndex`, `SecretDivide`, `VariableTime`),
  but the summary that gives them is not the walk's `EffectSet`: it is a per-instance taint
  summary, which arguments' secrecy flows to which results and which of them reach a branch,
  an index, a division or a call of a variable-time intrinsic. A summary per instance, memoized
  the same way, composed at each call site by substituting the callee's parameter taints. A
  call's summary is a function of argument taints, not a set, so it is its own query beside
  `instance_effects`, keyed the same (`(krate, body, args)`), and reuses the walk's resolution of
  each call site (dispatch, drop glue, closures) rather than walking again.
- It is checked on the optimized MIR the code generator lowers, never on the runtime MIR: a
  branch the optimizer turns into a `select`, or one it makes from a `select`, changes the
  answer; and LLVM can do the same below MIR, so the proof also pins the backend's flags for
  marked functions (`-x86-cmov-converter=false` and the like), recorded in the proof.

## Not done

- `for` loops, and every loop but a counted `while`, need `diverge` (above): the `for` rule
  over ranges and slices is next, then `decreases`.
- A `for const` loop of generic MIR (`ComptimeFor`) is a loop like any other to a generic
  check, though it runs once per field.
- Every call through a function pointer or an unlisted `dyn` needs `diverge`, whether or not
  its targets could reach the caller: no whole-program graph of indirect calls is built.
- A malformed `#![blaze::expect(..)]` on a module is read as the words before its error and is
  not reported itself; a function's is.
- The edition gate is `#![blaze::effects]` until `edition = "blaze-2026.11"` exists.
- No consumer keys on `derived_pure` yet.
- The blocking set is rows of the toolchain and five registry crates; any other crate's blocking
  wrapper over a foreign call is found through the foreign table (its `read`, `futex`), so the
  table is complete through its foreign leaves, but the message names the leaf.
- A trusted mark is per effect: a `pure(trusted)` function is still walked for blocking,
  allocation and panics.
