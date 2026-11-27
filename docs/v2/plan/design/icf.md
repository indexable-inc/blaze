---
status: plan
summary: Identical code folding in blaze. Built - one round in blaze-export's layout places records with equal bytes once and gives every folded symbol the kept copy's address. Still to build - the fixpoint (a caller of two folded callees folds a round later), and the functions that never fold (significant addresses, slot-reached live code, counted code, statics). Also the guarantee folding relies on.
date: 2026-11-02
---
# Identical code folding

Two instances whose code is equal are one function in a blaze build: one copy in the image, one
address.

## What exists

`crates/blaze-export/src/layout.rs`, in `lay_out_with_tls_par`: after ordering, each record is hashed
(`FOLD_DOMAIN`, BLAKE3, in parallel) and a record whose bytes (code, relro part, targets, names)
equal an earlier one's is not placed again. Its symbol keeps its own name and gets the kept
copy's address (`Placed::folded`). The time is `fold_ns`, in `blaze build`'s link-phases line. A target is
named by symbol, never an address, so equal code with equal targets folds on every machine.

## Not built

### The fixpoint

Folding renames every relocation that named a function folded away, so two callers that differed
only in which of two equal callees they called become equal one round later. Folding repeats
until a round folds nothing, as lld's and mold's ICF do.

- **Bounded.** Every round but the last folds at least one record, so `records + 1` rounds always
  suffice. A round past that is an error naming the rounds, the records and a pair the last round
  folded; the build stops.
- **Cost.** A later round rehashes only the records its renames changed.
- **Deterministic.** Each group keeps its first record in layout order, so the image is the same
  whatever order the build made its records in.
- **What is renamed.** Each function's code relocations, its data's relocations and its unwind
  personality or handler; statics' values (`static TABLE: [fn() -> u32; 2] = [f::<u32>,
  f::<i32>]` names both instances); and data, in one forward pass, since a datum names only data
  before it. A static's writable datum keeps an identity of its own, so it stays its static's
  object.

### What never folds

- **A function whose address is significant**: one with a symbol (`#[no_mangle]`,
  `#[export_name]`, an exported `extern "C" fn`, the entry point's callee). Another language may
  compare its address or take it by name.
- **A function of a live build reached through its slot**: an edited one, a rebound one and a
  tiered one. Each keeps its slot, which an edit or a tier-up replaces alone.
- **A covered, counting or PGO-instrumented function**: folded, two would share counters.
- **Statics, never.** `#[used]` applies only to statics, so it is covered. Two statics' equal
  mutable bytes stay two objects. Read-only data are one per content, as rustc's `unnamed_addr`
  constants are.

## The guarantee relied on

Rust does not guarantee that distinct functions have distinct addresses:

- rustc declares every Rust function `unnamed_addr`, with this comment in
  `rustc_codegen_llvm::declare::declare_fn`: "Function addresses in Rust are never
  significant, allowing functions to be merged". So LLVM's MergeFunctions (on by default in
  optimized builds, `-Z merge-functions=aliases`) and lld's `--icf` may give two functions one
  address.
- The standard library's [`fn` docs](https://doc.rust-lang.org/std/primitive.fn.html):
  "comparing function pointers is unreliable: pointers to the same function can compare
  inequal (because functions are duplicated in multiple codegen units), and pointers to
  different functions can compare equal (since identical functions can be deduplicated within
  a codegen unit)".
- [`std::ptr::fn_addr_eq`](https://doc.rust-lang.org/std/ptr/fn.fn_addr_eq.html): the comparison
  "May return `true` unexpectedly, in cases where functions are equivalent ... This is
  particularly likely with generic functions".

So folding a Rust function whose address Rust code compares is allowed, and the program
behaves as one rustc's optimized build may (rule 4). What the program can rely on still holds:
a pointer to a function equals itself, and two pointers that compare equal call the same code.
The functions whose distinctness something outside Rust may rely on are the exported ones, and
they never fold.

One caveat for later: under CFI (`-Zsanitizer=cfi`/`kcfi`), rustc attaches a type ID to each
function, and folding two with different type IDs would break the check. blaze doesn't build
with CFI. When it does, the type ID goes into the folded bytes.

## Open

- **Lines.** If a record holds the function's line table, two *different* source functions with
  equal code never fold in a debug build. That keeps backtraces true. A release build without
  line tables folds them.
