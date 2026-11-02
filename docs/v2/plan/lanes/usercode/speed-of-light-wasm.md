---
status: superseded
summary: The speed of light of wasm proc macros (blaze-macro-wasm) and of WASI build scripts (blaze-wasi), covering loading a module, an expansion in a fresh instance or a session, the guest's proc_macro and the WASI host. Each section counts the work, gives the bound and today's count, and says what closed the gap or what stands in the way.
date: 2026-10-30
superseded_by: ../dedup.md#proc-macros-one-engine-blaze-macro-eval
---
# Wasm proc macros and WASI scripts: speed of light

Superseded on 2026-10-30: `blaze-macro-wasm` and `blaze-wasi` are deleted, and every proc macro
runs on blaze's evaluator, `blaze-macro-eval` ([dedup.md](../dedup.md#proc-macros-one-engine-blaze-macro-eval)).

Part of [usercode's speed of light](speed-of-light.md). Code: `crates/blaze-macro-wasm/` (the host
`src/lib.rs`, the WASI host `src/wasi.rs`, the guest `guest/proc_macro.rs`, `src/bench.rs`)
and `crates/blaze-wasi/` (`src/run.rs`, `std/process.rs`). Nothing here has been measured:
nothing compiles until every lane is done. The counts come from reading the code, and `proof`'s
budgets (the last section) hold them once it builds.

## The machine (speed-sol2-usercode-wasm)

The figures of [the link lane's speed of light](../link/speed-of-light.md), as the parent's
table has them, estimates and not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| page-cache copy (`read`, `write`) | about 10 GB/s a core |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Wasm figures, estimates too:
- a call from wasm into a `func_wrap` host function, or back, about 30 cycles (10 ns);
- a minor page fault on fresh or copy-on-write memory, about 0.25 µs;
- `munmap` of a mapping a multithreaded process has touched, about 5 to 30 µs (the TLB shootdown
  reaches every core the process runs on);
- SHA-256 with the SHA extensions about 2 GB/s;
- Wasmtime's own figure for a pooled instance from an `InstancePre` with a copy-on-write memory
  image is about 5 µs.

Reference modules: a small derive (about 300 KB of wasm, 1 MB of machine code) and a macro
built on syn (about 3 MB of wasm, 10 MB of machine code); a call's input of 2 KB of tokens,
about 100 tokens at 16 bytes of columns and 5 of text each on the wire
(`crates/tt/src/storage/wire.rs`: two words a node and two a packed span).

## Loading a module (speed-sol2-usercode-wasm)

`Wasm::at`, `Wasm::known`, `prepare`, `prepare_soon`, `load`, `linked`, the machine code kept
under `code_key` in the store.

Work per module, once a process:
- **Known digest** (a listing written by `prepare`/`prepare_soon`): one `stat` and read of the
  listing, and the store asked whether it has the code (`has_code`). The module's bytes are not
  read.
- **Unknown digest**: the bytes read (open, `statx`, read, read to end) and hashed with SHA-256:
  3 MB at 2 GB/s is 1.5 ms, which is the bound for a module nothing has listed.
- **Cold**: Cranelift's compile with parallel compilation, seconds of CPU for the syn module.
  It runs once per module digest and kind of host, off the critical path (`prepare_soon`
  starts it as the proc-macro crate links). Its code goes in the store and every later process
  and machine of the same kind loads it.
- **A hit on the store**: `get_lasting` reads the code into a fresh `Vec` (10 MB at 10 GB/s,
  1 ms) and checks its checksum (another pass, about 1 ms), then `Module::deserialize` copies it
  into an executable mapping (1 ms, plus 2,500 page faults, 0.6 ms). That is about 3.5 ms.
- **Linking**: `linked` makes a `Linker` with about 70 host functions (WASI and `add_host`),
  stubs the rest with `wasi::rest`, and makes one `InstancePre`, about 70 µs. It runs once per
  module and engine, about 2% of the load.
- **Lookups after that**: by digest, in a `Sharded` table of `OnceLock`s, with no lock and no
  allocation (`a_compiled_module_is_found_by_its_digest_with_no_allocation`).

The bound for a hit is Wasmtime's `Module::deserialize_file`: the code is mapped copy-on-write
(a few µs), its pages fault in only as they run, and a checksum is checked once per pack rather
than per read. That is about 0.1 ms for the syn module, so today's 3.5 ms is 35 times the bound.
- **Changed (speed-sol2-usercode-wasm)**:
  - `keep_code` now does a plain load of its `kept` flag before the swap. Every lookup of a
    module passes through it, and the swap wrote the shared line each time.
  - The exports an instance is used by (`memory`, `_initialize`, the alloc and expand exports)
    are looked up by name once per module (`Exports`, as `ModuleExport` indices), not once per
    instance.
  - `code_key` takes the engine, since a module may be compiled for the pooled engine (next
    section). The two engines differ only in how they allocate instances, which is not part of
    `precompile_compatibility_hash`, so one key and one copy of the code serve both. A test
    holds the keys equal.
- **In the way**: the store gives a value only as a copied `Vec`. A page-aligned, verified
  mapping of it (or an open `File`) would let `Module::deserialize_open_file` load in place.
  Request [00928](../../../../../v2/queue/00928.json) to lane store asks for this, in blaze-cas,
  which replaces blaze-store at integration.
- **`macros.md` says** the code "comes from the store as bytes and is copied in", which is
  right; this section adds the counts.

## An expansion: instances, the boundary, the verdict (speed-sol2-usercode-wasm)

`expand` (a module whose expansions keep no state: a fresh instance per run), `Session` (a
stateful or unseen module: one instance per crate and revision), `Calls`, `add_host`, `run`,
`placed`, `Questions`, `answer`, `Now`, the verdict (`Seen`).

### Making an instance

Before this round, every instance came from Wasmtime's on-demand allocator. Per fresh
instance:
- `Store::new` and the host state;
- an `mmap` reserving 4 GiB plus guard, and the copy-on-write image mapped into it;
- `_initialize`, and two exports looked up by name;
- a page fault on every page the expansion touches: about 256 for a 1 MiB working set, 64 µs;
- an `munmap` on drop, 5 to 30 µs in the daemon.

That is about 80 to 120 µs before the macro's own work, 16 to 24 times Wasmtime's 5 µs.

The bound is Wasmtime's pooling allocator with copy-on-write memory images and an
`InstancePre`: about 5 µs, plus restoring the pages the expansion dirtied, which the work
itself sets (1 MiB at 32 B a cycle is 9 µs, less where `PAGEMAP_SCAN` finds only the dirty
pages). About 14 µs for the syn module.

**Changed (speed-sol2-usercode-wasm)**: a module whose verdict is stateless is compiled for a
second engine (`Kind::Fresh`, `engine_of`) with the pooling allocator (`pool()`):
- 1000 slots (`SLOTS`), each reserving its 4 GiB of address space untouched until used;
- 1 MiB kept resident per slot and reset in place (`linear_memory_keep_resident`);
- `pagemap_scan(Auto)`, so Linux 6.7 and later reset only the touched pages.

A fresh instance now costs no `mmap`, no `munmap` and no page fault on what the last instance
of the slot touched. Fresh instances wait for a free slot (`Slot`, counted under a mutex held
only to count, two uncontended lock pairs per instance, about 40 ns) instead of Wasmtime
refusing them: whether an expansion runs never depends on how many run at once.

Sessions stay on the on-demand engine. A session lives as long as its crate's revision, and a
build can hold more of them than any pool has slots. It is made once per crate and revision,
so its mapping is paid once for all of that crate's expansions.

The host's share of an instance:
- `State::empty`, which allocates nothing;
- `Files::new`: one allocation, the descriptors;
- `Environ::of`: one allocation, the environment's text;
- the package root's `PathBuf`.

It is about 0.3 µs on 14 µs, so a fresh instance is within 10% of the bound. On macOS, which
has no `memfd`, Wasmtime copies the data segments into the instance instead of mapping them.
The target machines are Linux.

### The boundary, per expansion

Each expansion crosses the boundary in these steps:
1. The host writes the input relative to the call (`wire::relative`, into a reused buffer).
2. It calls `alloc` (one call), whose guest side gets the room from the allocator and
   zero-fills it.
3. `memory.write` copies the input in.
4. Steps 2 and 3 again for an attribute, if there is one.
5. It calls `expand`. Inside, the guest reads the columns into its streams, runs the macro and
   writes its output's columns.
6. The guest calls `output`, which copies the bytes into `State::output`, a buffer the session
   reuses.
7. `wire::read` builds the tree in one checked pass.

That is 7 passes over a call's bytes where 6 are needed: the guest's zero-fill is the extra
one. A memset runs at about twice the rate of a copy, and the encode and decode passes run at
a few bytes a cycle, so the zero-fill is under 2% of the crossing. That puts the crossing
within 10% of its bound, which is the same steps without the zero-fill. For the 2 KB call it is
about 1 µs, against at least 10 µs for the smallest syn-based expansion. Removing the zero-fill
would need room handed out uninitialized from `__blaze::alloc`, which the safe `expand1`
could then be made to read. The guest's safety design refuses that (`input` finds its bytes
among the room `alloc` gave out).

Host calls made during an expansion:
- **Span words** are read with arithmetic in the guest. The host is called only for what it
  must answer: joins, parents, sources, the questions, and wide spans (`span_pack`,
  `span_unpack`).
- **Each host call** costs the 10 ns trampoline plus its own work.
- **The memory handle**: before this round, every host function that touches guest memory
  looked it up by name (`get_export("memory")`, a string hash and a map lookup, about
  50 to 100 ns). That covers `take`, `output`, `span_unpack`, `span_byte_range`, `ts_from_str`,
  `literal_from_str`, `panicked` and every WASI call. **Changed (speed-sol2-usercode-wasm)**:
  the handle is kept in `State::memory` when the instance is made, before any of its code
  runs, so a host call reads a field. The calls an expansion makes (`Calls`) are also found by
  index as the instance is made (`Calls::known`), with no lookup by name on the first
  expansion of a session.
- **`placed`** scans the output's string literals for `/blaze/package` only in a session with a
  package root, and rebuilds the tree only when one has it.

### Questions and the verdict

- **A question asked of a span** (source text, file, line and column, byte range) goes to
  `load_cargo::answer`. It is recorded with its answer as an input (`Value::read`, `said`): two
  strings per question, made only on a miss, which is the edge of keeping. A macro asks a few at
  most. On a hit, `Now` and the reused `Room` answer what a shape asks with nothing allocated
  but the output (`a_kept_expansion_is_found_with_nothing_allocated_but_its_output`).
- **The verdict** (`blaze_expand::verdict::Seen`, moved there by lane dedup): relaxed atomics
  per expansion, and a write to the store only when an expansion contradicts what it said, or
  at `flush`. That is at its bound.

### What the parent's section says

"A proc-macro expansion" in [speed-of-light.md](speed-of-light.md) is about a hit, which this
host does not change. `macros.md`'s table says wasm "falls short" of Wasmtime's pooling
allocator, with "on-demand allocation, one `mmap` per instance on a miss". That is no longer
true of stateless modules, which are pooled. It is still true of sessions, for the reason given
above.

## The guest's `proc_macro` (speed-sol2-usercode-wasm)

`guest/proc_macro.rs`: the module's imports (`sys`), its allocator (`Counted`) and its exports
(`__blaze`). The token API it is compiled with is `blaze-expand`'s `native/proc_macro/api.rs`,
owned by speed-sol2-usercode-macros ([speed-of-light-macros.md](speed-of-light-macros.md)
counts its per-token costs: `Rc` streams and a `Vec` per group, which a macro's owned
`TokenStream` per group needs).

Work per expansion in this file:
- `hook()`: one `set_hook`, a `Box` made and the last one freed, about 150 ns;
- `before()` and `after()`: a sum over at most two rooms, and the rooms freed;
- `input`: the room found by pointer among at most two;
- `give`: the output's columns written into a `Vec` that grows by doubling, at most one more
  copy of the output, often grown in place at the top of the heap.

Per allocation, `Counted` adds one relaxed add, which on `wasm32-wasip1` without threads is
a plain load, add and store: about 3 instructions on top of dlmalloc's 50 to 100.

The bound per expansion is the same calls with no `set_hook`, and a buffer that doesn't grow.
Today's code is under 0.5 µs above that, under 5% of the smallest expansion, so within 10%.
The hook is set again on every expansion on purpose: a macro may replace the panic hook, and
setting it again is what makes sure the host is told of every panic. Keeping the output
buffer between expansions would count as state the expansion left live (`after`), which would
mark the module stateful.

## The WASI host (speed-sol2-usercode-wasm)

### `blaze-macro-wasm/src/wasi.rs`: a module reading its package

Every call validates its guest pointers and reads the memory handle, which is a field now (see
"The boundary").

**`path_open` of a file `d` parts deep, `path_filestat_get` of one.**
- **Before**: `resolve` ran `canonicalize` on the whole path. Glibc's realpath does an `lstat`
  per part of the package's own path (about 7 parts, a registry checkout's) and of `rel`, and
  the root was canonicalized again on every call. Then a `stat`, then `fs::read` (open,
  `statx`, read, read to end, close). That is about 2 × 7 + d + 6 syscalls: 22 for d = 2,
  about 7 µs.
- **Changed (speed-sol2-usercode-wasm)**: `Files::links` does one `lstat` per part of `rel`
  only, and the last `lstat` serves as the `stat`, since a part that is no link has the same
  `stat`. The whole path and the root are resolved only when a part is a link, and the root
  once per instance (`canonical`). `read_whole` reads into one buffer of the size the `stat`
  gave: open, read, read to end, close, with no `statx` or `lseek` as `fs::read` and
  `File::read_to_end` make. That is d + 4 syscalls: 6 for d = 2, about 2 µs plus the read and
  its BLAKE3.
- **Bound**: an open that refuses links anywhere in the path (Linux `openat2` with
  `RESOLVE_NO_SYMLINKS`, macOS `O_NOFOLLOW_ANY`), then `fstat`, read, read to end and close on
  that descriptor: 5 syscalls at any depth. The existing realpath check would run only on
  `ELOOP`, for a path that does have a link, so the semantics stay exact. `RESOLVE_BENEATH` is
  not the same: it refuses an absolute link that points inside the package, which the module
  may follow today.
- **Gap**: d = 1 (`Cargo.toml`) is at the bound. d = 2 (`src/grammar.pest`,
  `.sqlx/query-*.json`, templates) is 6 syscalls against 5, about 12% of a call that reads a
  4 KB file.
- **In the way**: the workspace has no safe `openat2` (only `libc`, whose raw `syscall` would
  be a new unsafe module that can't be checked by a build in this round). Request
  [00929](../../../../../v2/queue/00929.json) asks the coordinator for `rustix`, which is in
  `Cargo.lock` already through wasmtime. The change is then about 30 lines in `resolve` and
  the `path_open` closure.

**`fd_readdir`.**
- **Before**: every call listed the directory again (open, `getdents`, close), made an
  `OsString` per name, sorted, and ran a `stat` per entry from the cookie on. wasi-libc reads a
  directory in 4 KiB calls, so a 200-entry directory took about 3 calls and about 600 `stat`s.
- **Changed (speed-sol2-usercode-wasm)**: a pass from cookie 0 lists the directory once, with
  each entry's type taken from the listing itself (`d_type`) and a `stat` only for a link,
  whose type is what it leads to, as before. The listing is kept on the descriptor (`listing`),
  and later cookies read it with no syscall and no allocation
  (`a_pass_over_a_directory_lists_it_once_and_gives_what_a_listing_per_call_gave`).
- **Bound**: the listing's own syscalls. At the bound.
- **The listing is recorded once per pass**, not per call. `trace::normalize` deduplicates
  equal records, so a key changes only where a directory changed between the calls of one pass,
  which the kept listing now reads as one consistent listing.

**`environ_get`.**
- **Before**: the environment was a `Vec<(OsString, OsString)>` per instance, two allocations
  per variable, and five bounds-checked writes per variable.
- **Changed (speed-sol2-usercode-wasm)**: it is made once per instance as one text with its
  starts (`Environ`), and `environ_get` is one copy and a table of pointers. That is at its
  bound. Nothing is written unless both fit, where the old code wrote some variables before
  refusing. A test runs every case against the old writer.

**The other calls are at their bound**:
- `fd_read` and `fd_pread`: one copy from the bytes the descriptor holds, with the iovec pairs
  in reused scratch;
- `fd_write`: one copy, up to 64 KiB kept;
- the clocks: the vDSO;
- `random_get`: `stdx::rand::fill`, lane dedup's.

### `crates/blaze-wasi`: a build script under WASI

`run.rs`, an experiment that is off by default (docs/cargo.md, "Build scripts under WASI").

**Compiling.**
- **Before**: every run compiled the script's module with Cranelift (`Module::from_file`).
  That is about 100 ms to 1 s for a script linked with std (1 to 3 MB of wasm), against a run
  that is usually 1 to 10 ms when it starts no compiler.
- **Changed (speed-sol2-usercode-wasm)**: `compile` keeps the machine code in the store, under
  a key of the module's bytes and the engine's `precompile_compatibility_hash` (`code_key`).
  Cranelift runs once per module and machine. A hit is a BLAKE3 of the bytes (3 MB at 6 GB/s,
  0.5 ms) and the copying load of the section "Loading a module", about 3 ms for 10 MB of code.
  Code the store refuses is an error naming it, never a compile in its place. A test runs a
  module compiled, loaded from the store, and compiled with no store, and gets the same exit
  each time.
- **In the way of the last step**: the same as for proc macros: request 00928.

**Linking.**
- **Before**: every run made a `Linker`, about 46 WASI preview 1 functions and 2 of blaze's,
  each a host function registered with the engine, about 50 to 100 µs.
- **Changed (speed-sol2-usercode-wasm)**: the state is no longer generic over the caller's
  answerer. It holds a `Box<dyn FnMut>`, called once per command the script runs, each a
  process started. So one `Linker` serves every run of the process (`linker()`), and a run only
  resolves its imports and instantiates: a few µs, within 10% of a 1 ms run.

**Per `run_tool` call** (`std/process.rs`), the work is:
- the request encoded once in the guest and read in place by the host, which copies only what
  it owns (`a_request_read_allocates_what_it_owns`);
- the answer written into a buffer reused for the run
  (`an_answer_written_warm_allocates_nothing`);
- the answer copied into the guest, and its stdout and stderr copied once more into the
  guest's `Vec`s;
- the memory export looked up by name.

All of it is nanoseconds against the process the call starts, which is milliseconds, so it is
within 10% of the call.

## What `proof` measures (speed-sol2-usercode-wasm)

- **Loading a module**: allocations of a lookup by digest (0, held by a test); time to load the
  syn module's code from the store (3.5 ms today, 0.1 ms once request 00928 lands); and bytes
  read and hashed per load (0 with a listing).
- **A fresh instance**: `blaze-engine proc-macro-wasm <module> --macro <name> --input <file>`
  prints `wasm fresh`, the median of an instance made from the pool, run and dropped. Its
  budget is the `wasm warm` of a session plus 15 µs. Also count `mmap` and `munmap` calls per
  fresh instance (0 after the first in a slot) and slots given back
  (`a_fresh_instance_in_the_pool_expands_as_a_session_does`).
- **The boundary**: `wasm warm` for the 2 KB input. Allocations of a warm session expansion are
  only its output's (held by a test); host calls that look memory up by name (0).
- **The WASI host**: syscalls per `path_open` of a file d parts deep (d + 4 today, 5 after
  request 00929); a readdir call that goes on from a later cookie allocates nothing (held by a
  test) and makes no syscall; the reads of a file read whole (open, read, read, close; at most
  one allocation, held by a test).
- **`blaze-wasi`**: Cranelift compiles per script module and machine (1); time to load a
  script's code from the store; one `Linker` per process (held by a test).
