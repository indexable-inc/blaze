---
status: plan
summary: blaze v2: the rules every part keeps.
date: 2026-10-29
---
# Principles

Non-goals: changing what Rust means (superset-of-rust holds); running rustc for anything
(no-rustc holds); a fallback when v2 can't do something (no-fallback holds: it stops and says
what and where); an IDE. The only interface is the agent API (CBOR over a socket or HTTP),
shaped for LLMs; the IDE crates (`ide-db`, `hir-ide`, `ide-diagnostics` and the rest) are not
a constraint, and what the compiler needs from them (diagnostics) moves into the compiler.

**Everything is ours, at every level, for the best performance possible.** Nothing is fixed:
not a crate, an interface, a data structure, a vendored dependency (salsa, cranelift, LLVM's
bindings, the trait solver, std's build), the toolchain, the kernel interfaces we use, the
file formats, the CI or the machines. If changing it makes blaze faster and keeps Rust's
semantics, change it. Any dependency v2 needs to change is in the workspace and changed in
place: salsa (`vendor/salsa`), cranelift, llvm-sys, `rustc_type_ir` and the trait solver are
already there; anything else is vendored the moment it's in the way.

**Fast in every line, not just in the design.** Every piece of v2 is written as the fastest
code that keeps Rust's semantics. That means arenas, flat columns and no lock on a read, the
way [data.md](../design/data.md) and [concurrency.md](../design/concurrency.md) say. The
checklist every agent writes to is in
[Fast code, line by line](#fast-code-line-by-line) below. Code that is correct but slow isn't
finished.

**Within 10% of the theoretical maximum, everywhere.** Every component has a speed of light,
the bound physics and the work itself set. Each part of v2 is designed to come within 10% of
it. Typical bounds:
- **memory-bound work:** bytes touched divided by memory bandwidth;
- **compute-bound work:** instructions needed divided by instructions per cycle times cores;
- **I/O:** bytes divided by device bandwidth, with syscalls batched to the minimum;
- **a build:** the larger of total work divided by cores and the critical path;
- **an edit:** recomputing only what changed, plus the fixed latency of each stage.

Each lane writes its components' bounds, with the arithmetic, in its notes
(`docs/v2/plan/lanes/<lane>/speed-of-light.md`), and how its design reaches 90% of each. Where
it can't, the notes say what stands in the way and what would remove it. `proof`'s budgets
measure every component against its bound once things compile, and a component more than 10%
off its bound is a bug.

**State of the art in every part.** Each component matches or beats the fastest known way to do
its job, and its lane names that benchmark in its notes. For example: the linker against mold
and wild, hashing against BLAKE3's SIMD paths, the store against content-addressed stores with
io_uring, interners against the best lock-free tables, parsing against simdjson-style
branch-free lexing, the scheduler against work-stealing runtimes, and incremental invalidation
against Unison and rustc's red-green graph, finer than both. Where we don't beat it, the notes
say why and what it would take.

**It just works, and nothing binds us to rustc.** The bar is that real programs build and run
under blaze, judged by the crate corpus and people's projects. rustc's choices are defaults we
follow where following is free and departing would cost users. They are not constraints, and
no part of v2's design may assume them.
- **Follow rustc by default where real code leans on it** and following costs nothing:
  semantics, which programs compile, trait selection, layout (crates assert sizes; we compute
  it with rustc's algorithm anyway), the `--print` output build scripts read, cargo's JSON.
- **Keep every such choice replaceable.** Layout, mangling, the solver's rules, and the formats
  are each one module behind an interface, so changing one is a local change. Changes come
  through an opt-in (a blaze edition or a crate attribute, AGENTS.md superset-of-rust), or by
  default once the corpus shows nothing breaks.
- **Be free everywhere nobody can tell:** machine code, dev symbol names, debug-info layout,
  file and cache formats, `target/` internals, diagnostic wording, unused `-Z` flags. Take the
  fastest way.

**Deterministic**: the same inputs give the same bytes, whatever the scheduling, the core count
or the machine. The invalidation oracle (section 11) depends on it.

**Profile first**: before the targets bind, the coordinator measures where today's 0.27 s body
edit and 35 s start go, and the targets are set from that.

Lane agents write code and tests and read; they don't run anything expensive. Allowed: reading
files, `rg`, `ls`, `wc`, read-only git (`diff`, `log`, `show`, `status`, `grep`, `ls-tree`),
`gh issue view` and `gh pr view`, and starting child agents. Not allowed: any build, check or test
(`blaze`, `cargo`, `rustc`, clippy, `nix build`, `nix develop`), harnesses, benchmarks, ix VMs,
downloads, long loops, and git commands that change state. The coordinator alone builds and tests,
all at once, in an ix VM.

**Finished code only.** Every lane writes its part end to end: every body, wired into the path a
real build takes, with no `unimplemented!`, `todo!`, stub or deferred piece. A lane isn't done
while any remain in its paths. v2 lands complete or not at all.

## Fast code, line by line

- **Memory**
  - **Arenas, not the heap.** A task allocates from its own bump arena, freed in one go when the
    task ends. Long-lived data goes in typed arenas indexed by `u32` IDs. Nothing on a hot path
    calls the global allocator per node, per item or per edge, and a cache hit allocates
    nothing.
  - **IDs, not pointers.** Things refer to each other by `u32` index into a column. No `Box`,
    `Rc` or `Arc` per node and no pointer-linked trees. `Arc` is for a few long-lived roots;
    never clone one in a loop.
  - **Columns, not structs of everything** (SoA, [data.md](../design/data.md)). Hot fields sit
    together and cold ones go to a side table. Small types carry a `const` size assertion:
    tag plus `u32` data, lists as CSR ranges, sets as bitsets.
  - **Interned once.** Names, paths and types are interned to IDs on the way in. After that,
    comparing or hashing one compares a `u32`, never a string.
- **Inner loops**
  - **No hashing twice, and the cheapest hash that fits.** Keys that are already content
    hashes index directly. Elsewhere use `FxHash` or a perfect index; SipHash is for untrusted
    input only. Use the entry API, never a lookup followed by an insert.
  - **No `dyn` and no `String` in inner loops.** Dispatch on a `u8` tag with a `match`, or batch
    by kind. Formatting and `String` building happen only at the edge (diagnostics, logs, the
    API).
  - **Preallocate.** `Vec::with_capacity` from a known count, `SmallVec` for the usual tiny
    case, and buffers reused across iterations instead of rebuilt.
- **I/O and the machine**
  - **Zero-copy.** Read mapped bytes in place: no deserialize-then-copy, no `Vec<u8>` per read,
    no `to_vec()` or `clone()` to satisfy a borrow.
  - **Batched I/O.** Many operations per syscall (io_uring, kqueue). No per-file `stat` and no
    blocking call on a worker thread.
  - **No lock on a read** and no shared cache line written on one
    ([concurrency.md](../design/concurrency.md)).
  - **Branch-light and SIMD where it pays**, chosen at run time, bit-identical across paths
    ([hardware.md](../design/hardware.md)).
- **Known names**
  - **Fixed name sets become enums.** Intrinsics, lang items, lints, attributes, keywords and
    target features are known when blaze is built. Each becomes a `u16` enum with a range of
    pre-interned symbols, so recognizing one is a range check on a symbol ID. Raw bytes from a
    lexer or an object file go through a perfect hash generated at build time. No `match` on
    strings and no `strip_prefix` dispatch on a hot path.
  - **Sets of known names become bitsets**: enabled lints per level, target features, cfg
    atoms.
- **Sorting and ordering**
  - **Integer keys use radix sort.** Addresses, offsets, IDs and ranks use LSD radix sort on
    8-bit or 11-bit digits, parallel above about 100k elements (the way mold sorts). A
    comparison sort is for small inputs or keys that aren't integers.
  - **Sorting by name for determinism uses an order key.** Each interned symbol carries a
    precomputed `u64` order key (its first 8 bytes big-endian). Compare those first, and fall
    back to bytes only on a tie. Never `to_string()` or `clone()` inside a sort key.
  - **Ordered maps for determinism become hash maps sorted once** where the output is written.
    Timers use a hierarchical timing wheel, not a `BTreeMap`.
- **Allocation, `dyn` and clones are controlled mechanically**, not by care alone:
  - **By construction.** Big types in hot crates don't implement `Clone`. IDs are `Copy`.
    Shared big values are interned and referred to by ID. Hot APIs take borrowed views
    (`&[T]`, `&str` from mapped bytes) and write into buffers the caller passes.
  - **Statically.** `tools/v2/hot_lint.py` flags `.clone()`, `dyn`, string and `Vec`
    allocation, `collect()`, per-value boxes and SipHash maps in the hot crates. A line is
    exempt only with a `// alloc:`, `// dyn:` or `// clone:` comment naming why the cost is
    needed there (an error path, setup, the edge). From the first compile, clippy's
    `redundant_clone`, `clone_on_ref_ptr`, `implicit_clone`, `unnecessary_to_owned`,
    `inefficient_to_string`, `format_collect`, `needless_pass_by_value`,
    `large_types_passed_by_value`, `box_collection`, `vec_box`, `rc_buffer` and
    `large_enum_variant` are errors in v2's crates.
  - **By test.** Every hot path has an allocation-budget test using `blaze-alloc-count`'s
    counting allocator: a memo hit, an interner hit, a store read, a VFS lookup, a name
    resolution, a stencil emit and a radix pass allocate nothing. A parse, a lowering or an
    inference allocates at most a stated, constant number of buffers. Going over budget fails
    the test.
- **Every hot structure states its cost**: a comment or a `const` assertion giving its size
  per element and its lookup cost. `proof`'s budgets hold each lane to them.
