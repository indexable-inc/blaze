---
status: plan
summary: The ix-prof profile format and interfaces. Covers the .ixprof container (canonical JSON manifest, Arrow IPC tables, blake3 id), the tables, symbols, pprof and Chrome exports, the /debug/pprof endpoints, the live stream with cursors, gaps and drop counts, and the design for profiling any blaze-built program with no code changes.
date: 2026-11-02
---
# ixprof: the profile format and API (v1)

## 0. The headline: profile any program blaze builds, with no code changes

`blaze run --prof`, `blaze test --prof` and `blaze bench --prof` profile the program blaze
built: CPU samples, a span per function call, allocations by stack and every blocking wait
named, with no `tracing` in the program. Manual `tracing` spans (section 8) are an optional
extra that adds names to the same timeline. (Flag name: cargo's `--profile <name>` already
means the build profile and blaze keeps cargo's flags, so the switch is `--prof[=open]`;
`BLAZE_PROF=1` is the env form. Decision for Andrew; the rest of this page says `--prof`.)

Everything here is opt-in through blaze's own codegen and its own std build: a program built
without `--prof` is byte-for-byte what it is today and behaves as under rustc
(superset-of-rust). Status of each piece is marked in its heading:
**implemented** = in `crates/ix-prof` now; **designed** = only this page.

### 0.1 Sampling (no code change) -- the runtime is implemented, the link-in is designed

- Runtime (implemented, `ix_prof::sampler`): a sampler thread, 99 Hz default (Go's 100, off by one so it does not beat with periodic work). Linux:
  `perf_event_open` task-clock per thread (all threads of `/proc/self/task`, so no
  registration), user call chains by frame pointer. macOS arm64: every thread from
  `task_threads` is suspended, its registers read, frame pointers walked inside its stack,
  resumed, nothing allocated or locked while stopped (ported from blaze-profile's sampler,
  which only sampled threads that recorded a span). Blocked threads are sampled too
  (`on_cpu = false`). Where neither works (perf refused, x86_64 macOS) `start` returns an
  error naming why; a SIGPROF/`setitimer` sampler with a signal-safe frame-pointer walk is the
  next driver for those (designed).
- Frame pointers. Today: blaze keeps them only on request. `Toolchain.frame_pointers`
  (`crates/blaze-codegen/src/settings.rs:56`, set from `-C force-frame-pointers` in
  `v2/toolchain.rs:236`) drives `backend::FRAME_POINTERS`
  (`crates/hir-ty-codegen/backend.rs:78`), which sets Cranelift-IR `preserve_frame_pointers`
  for the stencil path (`target.rs:514`) and LLVM's `"frame-pointer"="all"`
  (`llvm/lower.rs:571`). Stencil glue already builds a Cranelift-shaped frame (`rbp`/`x29`,
  `blaze-stencil/src/arch/{x64,aarch64}/frame.rs`). Hook: `--prof` sets
  `frame_pointers = true` for every crate including std (std is built by blaze from
  `library/`, so std frames unwind too). On aarch64 Apple targets the ABI keeps frame pointers
  always, so there the cost is already paid.
- Cost (the number): no measurement exists in the repo yet. Published measurements of
  `-fno-omit-frame-pointer` on x86_64 (Fedora 38's distro-wide change, Ubuntu 24.04, Brendan
  Gregg) put it at about 1% on average, 0-2% for most programs, up to ~10% for a few
  register-starved hot loops (CPython's interpreter loop); aarch64 Apple: 0 extra. Plan: make
  frame pointers always-on once `blaze bench` over the bench corpus shows the x86_64 cost
  below 1% (the measurement and its tree go in docs/profiling.md); until then they are on
  under `--prof` only.
- Line info: both backends write DWARF 4 line tables per function
  (`crates/hir-ty-codegen/debuginfo.rs`, fed1eb428d:docs/reports/DEBUGINFO.md; stencils record
  `(offset, srcloc)` → `LineRow`). `--prof` forces at least `debug = "line-tables-only"`, so
  `blaze pprof list` has lines. The symbolizer (implemented, `ix_prof::symbols`) reads the
  executable's symbol table and DWARF (`addr2line` over `gimli`), inline chains included.
- Link-in (designed): with `--prof`, blaze adds `ix-prof` (feature `runtime`) to the link of
  the final binary and an init-array entry (`.init_array` / `__DATA,__mod_init_func`) that
  calls `ix_prof::start_from_env()` before `main`; blaze sets `IX_PROF`/`IX_PROF_HTTP` for the
  child (section 9). This alone gives a working pprof.

### 0.2 Function spans that cost nothing when off -- designed

Two mechanisms, by build kind:

- **Live (dev) builds already call through a jump table** (`crates/blaze-live`, every
  replaceable function goes through a slot `blaze-link` emits, `write.rs:670`; switching is one
  atomic store, `slots.rs`). Turning profiling on for function `f` = store into `f`'s slot the
  address of a thunk that records enter (fn id, time), calls the old target, records exit. No
  sled, no extra cost off beyond the indirection those builds already have. Thunks are made by
  the daemon like any patch (`blaze-live/src/patch.rs` `Code`), selected by size
  (`--prof=fns:>64` = functions whose code is over 64 bytes) or by path pattern.
- **Release / non-live builds get sleds**: at entry, after the frame is set up, one patchable
  nop (x86_64 `0f 1f 44 00 00`, 5 bytes = room for `call rel32`; aarch64 one `nop`, room for
  `bl` to a per-module trampoline island within ±128 MiB) and the same before each `ret`.
  LLVM path: LLVM's own XRay (`"function-instrument"="xray-always"` /
  `"xray-instruction-threshold"="N"` beside `frame-pointer` in `llvm/lower.rs:571`) on ELF;
  on Mach-O, where LLVM's XRay is not supported, `"patchable-function-entry"="1"` plus exit
  handling by return-address hijack (the entry hook swaps the return address for a
  trampoline and keeps the real one on a per-thread shadow stack, as uftrace does). Stencil
  path: the sled is a glue piece emitted in the prologue/epilogue glue
  (`blaze-stencil/src/arch/*/glue.rs`). Every sled is listed in a section `ix_prof_sleds`
  (fn address, sled offsets, kind, code size, name string), which the runtime patches with
  `mach_vm_protect`/`mprotect` + write + icache flush, all threads running (a 5-byte nop is
  replaced by a single aligned store; the sled is 8-aligned so no thread sees a torn
  instruction). `docs/support.md:344` refuses rustc's user-facing `patchable-function-entry`
  attribute; this is blaze's own, under `--prof` only, so programs without it are unchanged.
- Cost off: one nop per call and per return in `--prof` builds; nothing in other builds.
- Output: `kind = "fn"` spans in the same `spans` table (`name` = the demangled path, `file`
  /`line` = the definition), plus per-function `calls` counts; the same stream (6a).

### 0.3 Allocations -- wrapper implemented, shim hook designed

- Implemented: `ix_prof::alloc::Counting<A>` wraps any `GlobalAlloc`: per-thread byte and
  count counters (feed `spans.alloc_bytes`), Poisson-sampled stacks (Go's 512 KiB mean) with
  a fixed table of sampled live blocks, so `allocs` has alloc vs live by stack.
- Hook (designed): blaze already generates the allocator shim
  (`crates/hir-ty-codegen/v2/allocator.rs`, `ALLOCATOR_ENTRIES` in `mod.rs:2003` mapping
  `__rust_alloc*` to `__blaze_global_alloc*`). Under `--prof` the shim's four entries call
  `ix_prof::alloc::{alloc, dealloc, realloc, alloc_zeroed}` with the program's allocator (its
  `#[global_allocator]` or std's `System`) as the inner one, so no program change.

### 0.4 Waits named for free -- designed

blaze builds the real std from its own copy (`library/`, `crates/blaze-cargo/src/toolchain/library.rs`,
docs/toolchain.md). Under `--prof` it builds a second flavor of that std with
`cfg(blaze_prof)` (its own `DIGEST`, so the store keys differ), where each blocking point
calls two hooks that std declares and `ix-prof` defines:

```rust
// in core::profile (blaze_prof only); weak no-op defaults in std, strong ones in ix-prof
extern "Rust" {
    fn __ixprof_wait_begin(kind: u8, object: usize, type_name: &'static str,
                           at: &'static core::panic::Location<'static>) -> u64;
    fn __ixprof_wait_end(token: u64);
}
```

Points (only the slow, blocking paths, so an uncontended lock costs nothing extra):
`sys::sync::mutex::*::lock_contended`, `RwLock` read/write contended, `Condvar::wait*`,
`sync::mpmc` blocking `recv`/`send` (`context::wait_until`), `thread::JoinInner::join`,
`thread::park`/`sleep`, `Once` wait, blocking `read`/`write`/`accept`/`connect`/`recv` in
`sys::fd`/`sys::net` (marked a wait only when it actually blocked: begin before the syscall,
end after, and the recorder drops waits under 1 µs into a counter so fast IO does not flood).
The object is named by its address plus `type_name::<T>()` and the `#[track_caller]`
location of the call that blocked; where the object was created is kept in a side table
keyed by address the first time it blocks (no field added to `Mutex`, so layouts stay
rustc's). The wait becomes a `spans` row with `wait` = kind, `wait_on` =
`"std::sync::Mutex<my::Cache> @ 0x7f..  (src/cache.rs:41)"`, and the owner where std knows it
(a futex mutex does not; `JoinHandle` knows the thread).

Async: the `Future::poll` of each `async fn`/block is a coroutine instance blaze compiles
(`crates/hir-ty-codegen/mod.rs:693`, MIR from `hir-ty/src/providers.rs:261`, suspension
points from `hir-ty/src/mir/held.rs`). Under `--prof` the poll body is wrapped: enter a span
`poll <fn path>` with `task` = the pinned future's address and, on `Pending`, the await
point's source location (the resume state's span) recorded as `wait = "await"`, `wait_on` =
that location. A task's lane in the explorer is its address from first poll to `Ready` or
drop.

### 0.5 Attaching to a binary already built -- designed

A `--prof` build carries the sleds and the runtime but does not start it unless asked:
`IX_PROF=<dir>` / `IX_PROF_HTTP=<addr>` start it at the init-array hook. For a live (dev)
build without `--prof`, `BLAZE_PROF=1` makes the blaze-live runtime ask the daemon for the
profiler as a patch (thunks in the jump table + the sampler loaded as patch code), so a
running dev program can be profiled without a rebuild. A release binary built without
`--prof` has no sleds; only the sampler can attach (by `LD_PRELOAD`/`DYLD_INSERT_LIBRARIES`
of the `ix-prof` cdylib, which starts at its constructor).

### 0.6 One stream

All of the above lands in the same tables (spans with `kind` fn/span/wait/poll, cpu_samples,
allocs) and the same live stream (6a), which `blaze run/test/bench --prof` expose and the
explorer reads.


Lane: v2-pprof (2026-11-01). Owner of the format: the `ix-prof` crate
(`crates/ix-prof`, `src/format.rs` is the code form of this page). The ix.dev explorer is one
reader; `blaze pprof` is another; `go tool pprof`, speedscope and Perfetto read the exports.

Any change that changes a column's meaning or removes one bumps `format_version`; adding a
nullable column at the end of a table, or a new table, does not.

## 1. What a profile holds

One run of one program (or several processes merged, e.g. a blaze build), with four kinds of
data that share one stack table and one thread table:

| kind | table | weight |
|---|---|---|
| CPU samples | `cpu_samples` | one sample = `period_ns` of CPU (or of wall, off-CPU) |
| span tree (tracing spans) | `spans` | wall `dur_ns`, own CPU `self_cpu_ns`, bytes/allocs |
| waits | `spans` rows with `wait` set | wall `dur_ns` |
| allocations | `allocs` (aggregate by stack) + `alloc_events` (sampled, timeline) | bytes and counts, total and live |
| counters | `counters` | value over time |

## 2. The file: `*.ixprof`

One self-describing, content-hashed, deterministic file. Layout (all integers little endian):

```
offset 0   8 bytes   magic  "IXPROF\0" 0x01        (the last byte is the container version, 1)
offset 8   u32       manifest_len (bytes)
offset 12  manifest  UTF-8 JSON, canonical (see 2.2), manifest_len bytes
           zero padding to the next multiple of 8
sections   each an Arrow IPC *file* (starts and ends with "ARROW1"), 8-byte aligned,
           zero padding between them; offsets in the manifest are absolute from byte 0
```

A reader: read 12 bytes, check magic, read the manifest, then for each table it wants slice
`[offset, offset+len)` and hand it to an Arrow IPC file reader (`apache-arrow`'s
`tableFromIPC(bytes)` in JS, `arrow_ipc::reader::FileReader` in Rust). Sections are
uncompressed Arrow (no LZ4/ZSTD buffer compression: arrow-js cannot read it). A whole file may
be wrapped in zstd as `*.ixprof.zst` (readers check for the zstd magic `28 b5 2f fd` first).

Arrow types used: `UInt16 UInt32 UInt64 Int64 Float64 Boolean Utf8` and
`Dictionary<UInt32, Utf8>` (marked `dict` below). No List, Struct, Union or LargeUtf8 (keeps
arrow-js simple). 64-bit integers arrive as `BigInt` in JS.

### 2.1 Manifest

```json
{
  "format": "ixprof",
  "format_version": 1,
  "id": "<blake3 hex, 64 chars>",
  "meta": {
    "program": "myapp",                     // the executable's file name, never argv (may hold secrets)
    "producer": "ix-prof 0.1.0",            // or "blaze 0.x (blaze-profile)"
    "clock": "darwin CLOCK_UPTIME_RAW",     // what every *_ns instant counts on
    "start_ns": 123456789,                  // first instant covered, on that clock
    "end_ns": 223456789,                    // last instant covered
    "start_unix_ns": 1793500000000000000,   // the same instant as start_ns on the wall clock
    "arch": "aarch64", "os": "macos",
    "cpu_period_ns": 1000000,               // null when no CPU samples; one sample's weight
    "cpu_sampler": "on: 99 Hz, thread_suspend + frame pointers",  // or "off: <why>"
    "alloc_sample_bytes": 524288,           // null when no alloc stacks; mean bytes between samples
    "alloc_counter": "on" ,                 // "on" when the counting allocator was installed, else "off: <why>"
    "symbolized": true,                     // false: frames hold addresses only (see 4)
    "labels": { "k": "v" }                  // free, producer-given (git commit, build id, ...)
  },
  "tables": [
    { "name": "threads", "offset": 4096, "len": 1234, "rows": 7, "blake3": "<hex>" }
  ]
}
```

`tables` is ordered by name. A table that has no rows is left out of `tables` (readers treat a
missing table as empty).

### 2.2 Canonical JSON and the id

Canonical JSON: object keys sorted by byte order, no whitespace, integers as JSON integers
(u64 values above 2^53 are still written as integers; JS readers parse the manifest with a
BigInt-safe parser or only use `offset/len/rows`, which stay small), strings with the minimal
escapes (`\" \\ \n \r \t \u00XX` for other control bytes).

`id` = blake3 of `canonical_json(meta)` followed by the `blake3` field of each table in
`tables` order (each as 32 raw bytes). So the id names the content, not the path, and two
writes of the same recorded events give the same id and the same bytes.

### 2.3 Determinism

A writer sorts before writing; no output depends on hash-map order, thread timing or
addresses of the writer's own data:

- `frames` sorted by (`module`, `address`, `inline_depth`, `function`, `file`, `line`); ids are
  row indices after the sort.
- `stacks` is a trie; nodes are sorted by their path of frame ids (root first, lexicographic),
  ids are row indices after the sort, so a node's parent always has a smaller id.
- `cpu_samples` by (`process`, `thread`, `t_ns`, `stack`); `spans` by (`process`, `start_ns`,
  `span_id`); `allocs` by (`stack`, `span_name`); `alloc_events` by (`process`, `thread`,
  `t_ns`); `counters` by (`name`, `process`, `t_ns`); `threads` by (`process`, `thread`).
- Dictionaries hold values in first-use order of the sorted rows.

## 3. Tables

`process` (u32, the pid) and `thread` (u64, the OS thread id: `gettid` on Linux,
`pthread_threadid_np` on macOS) together name a thread; `thread` and `span_id` (a counter from 1)
stay below 2^53, so a JavaScript reader may hold them as numbers; a span is named by
(`process`, `span_id`). All `*_ns` instants are on `meta.clock`; durations are nanoseconds.

### `threads`
| column | type | null | meaning |
|---|---|---|---|
| process | u32 | | pid |
| process_name | dict | | the program, or a role (`blaze serve`, a build script's crate) |
| thread | u64 | | OS thread id |
| thread_name | dict | yes | the thread's name |
| cpu_ns | u64 | yes | the thread's total CPU time over the profile, when known |

### `frames`
One row per (code address, inline level). An address with inlined calls has one row per
level: `inline_depth` 0 is the function the machine code belongs to, 1 the function inlined
into it at that address, and so on. In `stacks`, the deepest inline level sits innermost.
| column | type | null | meaning |
|---|---|---|---|
| function | dict | | demangled name without the hash (`{:#}` of rustc-demangle), e.g. `myapp::parse::<u8>`; for a span frame, the span's name; for an unsymbolized address `0x1234` |
| system_name | dict | yes | the mangled linker symbol (`_ZN...`, `_R...`) |
| file | dict | yes | source path as in DWARF (absolute, or relative to `comp_dir` resolved) |
| line | u32 | yes | 1-based source line of this frame's call or sample point |
| column | u32 | yes | |
| start_line | u32 | yes | the line the function's definition starts at (for `list`) |
| module | dict | yes | binary or shared library path; null for span/wait frames |
| build_id | dict | yes | the module's build id / Mach-O UUID, hex |
| address | u64 | yes | address relative to the module's load base (the file's address), so it is stable across runs with ASLR |
| inline_depth | u16 | | 0 for the outermost (the real function) |
| kind | dict | | `native` (machine code), `span` (a tracing span), `wait` (what a wait span waits on), `alloc` (an allocation site with no native stack) |

### `stacks`
A trie of call stacks, root (outermost frame) first.
| column | type | null | meaning |
|---|---|---|---|
| parent | u32 | yes | the caller's node; null at a root |
| frame | u32 | | row of `frames` |
| depth | u16 | | 0 at a root |

A stack id refers to its innermost node; walking `parent` gives the stack inner to outer.

### `cpu_samples`
| column | type | null | meaning |
|---|---|---|---|
| process | u32 | | |
| thread | u64 | | |
| t_ns | i64 | | when it was taken |
| stack | u32 | | innermost node in `stacks` (native frames only) |
| span_id | u64 | yes | the innermost span the thread was inside |
| on_cpu | bool | | false: the thread was blocked (macOS samples blocked threads too); its weight is wall, not CPU |
| weight_ns | u64 | | normally `meta.cpu_period_ns` |

### `spans`
The span tree, per thread. A span that moved threads (a fiber job resumed elsewhere) keeps
the thread it was made on.
| column | type | null | meaning |
|---|---|---|---|
| process | u32 | | |
| thread | u64 | | |
| span_id | u64 | | unique in its process |
| parent | u64 | yes | enclosing span; null at a root |
| depth | u16 | | spans its thread was inside when it began |
| name | dict | | span name |
| target | dict | | tracing target (module path unless set) |
| kind | dict | | `span` (a tracing span), `fn` (a function call from a sled/thunk, 0.2), `wait` (a wait: `wait` is set), `poll` (an async poll, 0.4) |
| task | u64 | yes | for `poll`/`await`: the future's address, the task's identity |
| file | dict | yes | callsite file from tracing metadata |
| line | u32 | yes | callsite line |
| start_ns | i64 | | |
| dur_ns | i64 | | wall time open (busy + idle + children) |
| busy_ns | i64 | yes | wall time it was entered (tracing enter/exit), children included |
| self_cpu_ns | i64 | yes | thread CPU time while entered, less that of child spans; ~0 for a wait; null when not counted |
| alloc_bytes | u64 | yes | bytes the thread allocated while entered (needs the counting allocator) |
| allocs | u64 | yes | allocation count, same |
| frees_bytes | u64 | yes | bytes freed while entered |
| wait | dict | yes | non-null: this span is a wait; the kind: `lock`, `channel`, `io`, `child`, `join`, `sleep`, `key`, `job`, `condvar`, `other` |
| wait_on | dict | yes | what it waits for (the lock's name, the key, the path, the child's pid) |
| wait_owner | dict | yes | who holds it / will wake it, when known |
| fields | utf8 | yes | the span's other fields, `k=v` pairs sorted by key, `\x1f` (unit separator) between pairs; values as `Debug`/`Display` printed |

Wait convention for producers (every-wait-says-what): a span is a wait when it has a field
`wait` (its value is the kind), with optional `on` and `owner` fields, e.g.
`info_span!("wait", wait = "lock", on = "cache", owner = %holder)`. blaze maps its own parks
(`switches` parked/resumed) to wait spans with kind `job`/`key`/`counter`.

### `allocs`
Aggregate by stack: the heap profile. With stacks (sampling on) one row per sampled stack;
with only the counting allocator, one row per innermost span name and `stack` pointing at a
`span`-kind stack (the span ancestry as frames).
| column | type | null | meaning |
|---|---|---|---|
| stack | u32 | | node in `stacks` |
| span_name | dict | yes | innermost span at allocation time |
| alloc_count | u64 | | estimated allocations (sampling scaled up, see below) |
| alloc_bytes | u64 | | estimated bytes allocated over the profile |
| live_count | u64 | | estimated allocations not freed at the profile's end |
| live_bytes | u64 | | estimated bytes not freed at the end (in use) |
| sampled | u64 | | raw samples behind the row (0 when exact) |

Sampling is Go's: a Poisson process with mean `meta.alloc_sample_bytes`; a sample of an
allocation of `size` bytes stands for `1 / (1 - exp(-size / rate))` allocations of that size.
The estimates are already scaled and rounded to integers; readers do not scale again.

### `alloc_events` (optional, timeline)
| column | type | null | meaning |
|---|---|---|---|
| process | u32 | | |
| thread | u64 | | |
| t_ns | i64 | | |
| stack | u32 | | |
| bytes | i64 | | positive: allocated (estimate the sample stands for); negative: the sampled block was freed |
| span_id | u64 | yes | |

### `counters`
| column | type | null | meaning |
|---|---|---|---|
| process | u32 | | |
| name | dict | | what is counted (`heap_live_bytes`, `rss_bytes`, producer-defined) |
| t_ns | i64 | | |
| value | f64 | | |

ix-prof writes `heap_live_bytes` and `heap_alloc_bytes_total` every 10 ms of profile when the
counting allocator is on.

## 4. Symbols and source lines

The writer symbolizes in-process when the profile is written (never while sampling): it reads
the executable's (and loaded shared libraries') symbol tables with `object`, maps each address
through DWARF line tables and inline info (`addr2line`/`gimli`: `function`, `file`, `line`,
inline chain), demangles with `rustc-demangle` (`{:#}`, no hash), and records `module`,
`build_id` and the module-relative `address`. On macOS the DWARF lives in the `.dSYM` next to
the binary or in the object files the debug map names; when neither is there, `file`/`line` are
null and `function` comes from the symbol table.

`meta.symbolized = false` means the writer only had addresses: `function` is `0x<addr>`, and
`blaze pprof` symbolizes later from the module path / build id if it can find the binary
(`--symbols <dir>`).

`blaze pprof list <regex>` reads `file` and `start_line`/`line` and prints the source with each
line's flat and cumulative weight; it looks the file up as recorded, then under `--source <dir>`.

## 5. Exports

### 5.1 pprof (`profile.proto`, gzip, `.pb.gz`)

`blaze pprof [--kind cpu|heap|spans|waits] in.ixprof export --pprof out.pb.gz`, and the
HTTP endpoints, write the standard `perftools.profiles.Profile`:

| kind | sample_type(s) | stack | period |
|---|---|---|---|
| cpu | `samples/count`, `cpu/nanoseconds` | native frames | `cpu/nanoseconds`, `cpu_period_ns` |
| heap | `alloc_objects/count`, `alloc_space/bytes`, `inuse_objects/count`, `inuse_space/bytes` (default `inuse_space`) | native (or span) frames | `space/bytes`, `alloc_sample_bytes` |
| spans | `calls/count`, `wall/nanoseconds`, `self_cpu/nanoseconds`, `alloc_space/bytes` | the span ancestry as frames (root span outermost), `function.name` = span name, `filename`/`line` = callsite | none |
| waits | `waits/count`, `wait/nanoseconds` | span ancestry + a leaf frame `wait <kind>: <on>` | none |

For `spans`, each span contributes its *self* wall time (dur minus children) and self CPU, so
`go tool pprof -top` sums correctly; `-cum` gives inclusive time.
Location ↔ frames: one `Location` per (module, address) with its inline chain as `Line`s
(innermost first, as pprof wants); one `Mapping` per module with `build_id`; one `Function`
per (name, system_name, file, start_line). Labels: `thread` (string, the thread name),
`thread_id` (num), `span` (string, innermost span name). `time_nanos` = `start_unix_ns`,
`duration_nanos` = end - start. Strings deduplicated in first-use order; the output is
deterministic too.

Import: `blaze pprof` also reads `.pb.gz`/`.pb` pprof files (from Go, pprof-rs,
rust-jemalloc-pprof, parca) into the same model (every sample type kept as a weight), so the
verbs and `diff` work on them.

### 5.2 Chrome trace / Perfetto (`.json`)

`export --chrome`: spans as `"ph":"X"` complete events (`pid`=process, `tid`=thread,
`ts`/`dur` in microseconds since `start_ns`, `args` = fields + self_cpu_ns), waits with
`"cat":"wait"`, counters as `"ph":"C"`, CPU samples as instant events (`"ph":"i"`, `"s":"t"`, cat `cpu`/`off-cpu`, name =
leaf function) on their thread, thread and process names as `"ph":"M"`.

### 5.3 speedscope

speedscope opens the `.pb.gz` export directly (it imports pprof).

### 5.4 Parquet for SQL

Designed, not built: `export --parquet <dir>` writes each table as `<dir>/<table>.parquet` (zstd), for DuckDB
(`SELECT ... FROM '<dir>/spans.parquet'`).

## 6. Live: the HTTP endpoint (`ix-prof` feature `http`)

`IX_PROF_HTTP=127.0.0.1:6060` (or `Config::http`), modeled on Go's `net/http/pprof`. Loopback
only; Unix only. Every request carries the token, 128 random bits made at start: loopback alone lets every
local account in. With `IX_PROF`, the address and the token are written to
`<IX_PROF>/http.endpoint`: one 0600 file, `<addr>\n<token>\n`, renamed into place whole so a
reader never pairs a new token with an old address, and removed at stop if it still holds this
session's token. stderr then says where the file is, not the token; without `IX_PROF` the
stderr line carries the token (`…/live?token=<token>`). The directory must be this user's own,
with owner-only mode and no ACL, on a local volume that keeps ownership and that this user or
root mounted: ix-prof makes it 0700
if missing and refuses, at start, one that is a symbolic link (trailing `/` and `.` stripped
first), belongs to another user, has any group or other mode bits, has an ACL entry (on macOS,
`acl_get_fd_np(ACL_TYPE_EXTENDED)` with an entry, which a directory inherits from a parent
another user owns; on Linux, a `system.posix_acl_access` attribute, or on the directory a
`system.posix_acl_default` one), or sits on a volume `fstatfs` shows ignoring ownership
(macOS `MNT_IGNORE_OWNERSHIP`), mounted by another user (macOS `f_owner` neither this user nor
root) or not local (macOS: no `MNT_LOCAL`; Linux: NFS, SMB, CIFS, SMB2, AFS or Coda by magic), or
on a Linux FUSE mount (`FUSE_SUPER_MAGIC`) whose `/proc/self/mountinfo` line, found by
`statx(STATX_MNT_ID)` and checked against the descriptor's device, shows neither `user_id` of
this user with no `allow_other` or with `default_permissions` (counted only when this user is
root or `/proc/self/uid_map` is exactly `0 0 4294967295`, since `user_id` is in the mounter's
user namespace), nor `user_id=0` with `default_permissions` on a type that is not a network
store (`fuse.sshfs`, `fuse.s3fs`, `fuse.rclone`, `fuse.gcsfuse`, `fuse.goofys`, `fuse.juicefs`,
`fuse.davfs`), nor type `virtiofs`; never changing its mode or ACL. On macOS, once the owner
check passes, it calls `fchown(fd, geteuid(), -1)` on the directory and refuses on error, which
turns a directory stored as uid 99 (shown as owned by whoever asks) into this user's own. Clients (`blaze pprof
--dir`, `--token-file`) apply the same directory rule, read only this user's own regular 0600
file with no ACL, of at most 64 bytes (never through a link, never a FIFO), require a loopback
IP address and port (for `--token-file`, the source URL's), and send the token only to that
address. A client sends it as
`Authorization: Bearer <token>`; a browser, which cannot set the header on the explorer's
fetches, as `token=<token>` in the query. A token on a command line is readable by every local
user (`ps`), so non-browser clients read it from `http.endpoint`. The server
thread stops and the port is released when the guard drops.

| GET | answers | content type |
|---|---|---|
| `/debug/pprof/` | a text index of the endpoints | text/plain |
| `/debug/pprof/profile?seconds=30` | CPU samples of the next N seconds, pprof | application/octet-stream (gzip pprof) |
| `/debug/pprof/heap` | allocations since start: alloc_* and inuse_*, pprof | same |
| `/debug/pprof/allocs` | same as heap with `alloc_space` as the default sample type | same |
| `/debug/pprof/spans?seconds=N` | the span profile (5.1 `spans`) of the last N seconds (default: all retained) | same |
| `/debug/pprof/waits?seconds=N` | the wait profile, the same window | same |
| `/debug/pprof/threads` | every thread with its open spans, innermost last (like goroutine?debug=2) | text/plain |
| `/debug/pprof/ixprof?last=N` | the window snapshot below, `format=ixprof|pprof|chrome` (`kind=` for pprof) | application/vnd.ix.prof |
| `/debug/pprof/live` | the stream, 6a | application/vnd.ix.prof-stream |

Only `profile` waits (Go's semantics: `seconds`, default 30, 1..3600); the rest answer at once
from what is retained. Every endpoint answers 403 unless `Host` is `127.0.0.1:<port>`,
`localhost:<port>` or `[::1]:<port>` (DNS rebinding) and the request carries the token (the
`Authorization: Bearer` header, else the query's `token`), and the server refuses to bind a
non-loopback address. Before the token check every local user can reach the parser, so it is
bounded: request line and headers together at most 16 KiB and whole within 5 s of the accept
(one deadline for the request, not a timeout per read), a 30 s write timeout, at most 32
connections waiting for their token check at once (one more evicts the oldest of them, shut
down; a connection that passed the check is never evicted), `%XX` decoded on bytes, and a failed `accept` pauses the listener (10 ms doubling to
1 s) instead of spinning.
CORS: answers carry `Access-Control-Allow-Origin` for `https://ix.dev` and loopback origins,
and answer the Private Network Access preflight
(`Access-Control-Allow-Private-Network: true`), so ix.dev/explore can fetch from
`http://127.0.0.1:<port>`.

`go tool pprof` cannot send the header, so a Go user fetches with `blaze pprof --dir <IX_PROF>
'profile?seconds=10' export --pprof cpu.pb.gz` and opens the file; `go tool pprof
'…&token=<token>'` works but puts the token on its command line. A browser client that adds
parameters (the explorer's `cursor=` on reconnect) keeps `token=`.

Window snapshot (what the live view asks for when it zooms or opens late): 
`GET /debug/pprof/ixprof?since_ns=<i64>&until_ns=<i64>` (either may be left out: from the
oldest retained / to now), or `?last=<seconds>`. It answers at once with an `.ixprof` (section
2) of what the retention ring still holds in that window; `meta.labels.window_complete` is
`"true"`, or `"false: <n> batches of <table> dropped before <t_ns>"` when part of the window was
already evicted. (`/debug/pprof/spans|waits` take `seconds` as the window; `since_ns/until_ns` there is designed.)

## 6a. Live: the streaming endpoint (`GET /debug/pprof/live`)

The one live stream of a running program. `blaze run --profile` and `blaze test --profile`
expose exactly this endpoint (section 9); the explorer's live view reads it with `fetch()` and
a `ReadableStream` reader (no WebSocket, no WebTransport needed; works through the same CORS
and Private Network Access headers as section 6).

```
GET /debug/pprof/live                      start from now (with the whole catalog, below)
GET /debug/pprof/live?from=start           everything still retained, then live
GET /debug/pprof/live?cursor=<token>       resume after the last frame the client saw
GET /debug/pprof/live?tick_ms=250          how often the server looks for new frames, 50..5000, default 250
(designed, not built: ?tables=spans,cpu_samples,... to send only some tables; ?catalog=1 to resend the catalog on resume)
```

Content type `application/vnd.ix.prof-stream`, chunked. The body is a sequence of frames:

```
frame   := len:u32 LE (bytes after this field) | kind:u8 | payload
kind 1  HELLO   payload = canonical JSON:
                { "session": "<hex>", "meta": { ...as manifest meta, no end_ns... },
                  "cursor": "<token>", "resumed": true|false, "retain_bytes": 67108864 }
kind 2  BATCH   payload = seq:u64 LE | name_len:u8 | table name (UTF-8) |
                one Arrow IPC *stream* (schema message, one record batch, end-of-stream marker)
kind 3  TICK    payload = canonical JSON:
                { "cursor": "<token>", "now_ns": i64, "seq": u64,
                  "dropped": { "<table>": u64, ... },          // cumulative since session start
                  "dropped_reason": { "<table>": "<why>" },     // e.g. "thread ring full (8192 samples)"
                  "first_retained_seq": u64,                   // oldest data BATCH still retained
                  "open_spans": u64 }
kind 4  GAP     payload = canonical JSON:
                { "from_seq": u64, "to_seq": u64, "tables": { "<table>": u64 batches },
                  "why": "cursor older than retention (64 MiB)" }
kind 5  END     payload = canonical JSON: { "why": "process exiting" | "server shutting down" }
```

Rules:

- **Tables and schemas** are those of section 3, unchanged, plus one live-only table,
  `open_spans` (the `spans` columns with `dur_ns` = now - start_ns and `self_cpu_ns`/allocs so
  far): a full replacement set sent every tick, also with zero rows when none is open (the
  page clears its set then), so the live timeline can draw spans that have not closed.
  `allocs` (the aggregate) is not in the stream: the stream carries `alloc_events` (sampled
  allocations, and frees as negative rows); the aggregate comes from a window snapshot. A span appears in `spans` once, when it closes.
- **Catalog tables** (`threads`, `frames`, `stacks`) are append-only within a session: ids are
  session-local, never reused or renumbered, and every `frames`/`stacks` row a batch refers to
  was sent in an earlier (or the same tick's earlier) BATCH. A `threads` row is re-sent when a
  thread's name changes; the latest row wins. Live ids differ from a file's ids (files sort,
  2.3); a window snapshot renumbers.
- **Order**: frames are written in `seq` order (u64, one counter for all BATCH frames of the
  session, starting at 1). A TICK follows each tick's batches; clients render on TICK.
- **Cursor**: `<session>.<seq>` (hex). `session` = blake3 of (pid, start_ns, start_unix_ns, a random
  u64 nonce) in hex, first 16 chars. Resuming with a cursor of the same session sends the catalog tables
  again only if the client asked with `catalog=1`, otherwise only what follows `seq`; a cursor
  of another session (the program restarted) is answered with `resumed: false` and a full start.
- **Bounded buffering, never silent**:
  - Each recording thread has a fixed ring; when it overflows, the oldest records go and
    `dropped[table]` counts them, with the reason.
  - The server keeps the last `retain_bytes` (default 64 MiB, `IX_PROF_RETAIN`) of encoded
    BATCH frames for resume and window snapshots; catalog tables are kept whole outside that
    budget. A resume older than that gets a GAP naming how many batches of which tables are gone,
    then continues.
  - A slow client: no per-client queue. Each connection is only a position (a seq) in the shared
    retention log; it reads from there at its own pace. A client that falls behind the retained
    bytes gets a GAP naming the seq range and how many batches of which tables it missed, then
    continues from the oldest retained batch; catalog batches are kept whole, so never missed. A
    client that wants the missed data asks for a window snapshot of `[from, to]`.
  - The program is never blocked by a client: the ticker thread encodes batches into the log
    under a short lock; the recorder's own rings and the allocator's sample queue
    (`crossbeam::queue::ArrayQueue`) are bounded, and a full one is a counted drop.
- **Several clients** may stream at once, each at its own position in the one log.

Window snapshot is the separate `GET /debug/pprof/ixprof?since_ns=..&until_ns=..` (section 6),
so the live view can backfill a zoomed range or a gap from the retention ring.

## 7. The explorer's entry points

- `blaze pprof <file|url> --http [addr]` serves `GET /profile.ixprof` (an .ixprof input),
  `GET /profile.pb.gz` (the cpu view, or the pprof input) and `GET /profile.chrome.json` on
  loopback (CORS and the Host check as above, and a new token per run that every request must
  carry, `token=` or `Authorization: Bearer`), then opens
  `https://ix.dev/explore#profile=<url-encoded http://127.0.0.1:PORT/profile.ixprof?token=TOKEN>`
  through a 0600 page in a new 0700 temporary directory, checked by the same rule as
  `http.endpoint`'s (the browser's command line holds the page's path, not the token).
  The explorer fetches that URL, parses the container (2) and draws it.
- The explorer may also accept a dropped/picked `.ixprof` file, and a URL to a live
  `/debug/pprof/ixprof?seconds=N` endpoint.
- The explorer's WebTransport feed from a blaze daemon (`blaze-explore`) is unchanged; a
  daemon's span/sample history can be pulled as an `.ixprof` with
  `blaze debug profile --ixprof out.ixprof` (designed, not built: blaze-profile's export).

What the explorer is expected to draw from these tables (the `blaze pprof` verbs compute the
same, so their numbers agree): flame graph / icicle of `cpu_samples` by stack (filter by
thread, span, time range); top functions flat and cum; the span timeline per thread (lanes),
with waits in a distinct style and `wait_on`/`wait_owner` on hover; span self-time tree; heap
`allocs` by stack with inuse/alloc toggles; counters as tracks; diff of two profiles (by
frame `function` names, since ids differ between files).

## 8. The Rust API (ix-prof)

```rust
// Cargo.toml: ix-prof = { features = ["http"] }       (all opt-in; nothing runs unless started)
#[global_allocator]                                   // optional: alloc counts and heap profile
static ALLOC: ix_prof::alloc::Counting<std::alloc::System> = ix_prof::alloc::Counting::new(std::alloc::System);

fn main() {
    // IX_PROF=<dir> on: records CPU samples, spans, allocations; writes <dir>/<program>-<pid>-<session>.ixprof
    // when the guard drops. Unset: returns a no-op guard, costs a branch per span.
    let _prof = ix_prof::start_from_env().expect("ix-prof");
    // or: ix_prof::start(ix_prof::Config { dir: Some(dir), cpu_hz: Some(99), ..Default::default() })?
    tracing_subscriber::registry().with(ix_prof::layer()).init();
    // ...
}
```

Environment: `IX_PROF=<dir>` (on, write there), `IX_PROF_HZ` (default 99 Hz; 0 turns the sampler
off), `IX_PROF_ALLOC=<bytes>|off` (sample rate, default 524288), `IX_PROF_HTTP=<addr>` (loopback only, token required), `IX_PROF_RETAIN=<bytes>` (live log, default 64 MiB).

The aggregation core (`ix_prof::core::Recorder`) is sans-IO: it takes events
(`span_new/record/enter/exit/close`, `cpu_sample`, `alloc`, `free`, `counter`) with times
from an injected clock (`advance_clock`) and gives `snapshot(since, until, symbolizer) ->
Profile` and `take_live() -> LiveDelta`; `Profile::to_ixprof()`, `Profile::from_ixprof(&[u8])`,
`View::of(&Profile, Kind)`, `View::to_pprof()`, `View::from_pprof(&[u8])`,
`chrome::to_chrome(&Profile)`, and the reports in `ix_prof::report`. The drivers (the sampler thread, the tracing layer, the allocator, the
HTTP server, the file writer) only feed it.

## 9. `blaze run --profile` / `blaze test --profile`

blaze links `ix-prof` into the program it builds (the opt-in extension; a program that does not
ask is unchanged), starts it before `main` and serves section 6 + 6a on a loopback port:

- blaze sets `IX_PROF_HTTP=127.0.0.1:0` (any free port) and `IX_PROF=<target>/profile/<run>`;
  the program's ix-prof writes the chosen address and the token to `<IX_PROF>/http.endpoint`
  (mode 0600; section 6), and to stderr only where that file is. blaze reads it, with the
  same checks as `blaze pprof --dir`, and prints
  `https://ix.dev/explore#live=<url-encoded http://127.0.0.1:PORT/debug/pprof/live?token=TOKEN>`
  (and opens it with `--profile=open`). The stream URL, token and all, is in the fragment,
  which the browser never sends to ix.dev's servers or CDN; and blaze opens it through a 0600
  page, as `blaze pprof --http` does (section 7), never with the link on a command line.
- At exit the program writes `<IX_PROF>/<program>-<pid>-<session>.ixprof` (the session id of
  6a, so a later run with the same pid never replaces it; the whole run, from the
  retention ring plus what was aggregated) and the stream sends END.
- `blaze test --profile`: one stream per test process; each test runs inside a span named
  `test` with field `name = <test path>`, so the live view lanes and the file both group by test.
- The explorer's live URL parameter is `#live=<stream url>`; the file one is `profile=<url>`
  (`#profile=` when the URL carries a token, as `blaze pprof --http`'s does). Both are read
  from the fragment, so a token in them never leaves the browser.

## Notes from the explorer lane (v2-explore-general, 2026-11-02)

Appended by the explorer lane; the format owner decides. Handoff: /tmp/blaze-lanes/v2-explore-general.md.

What the ix.dev explorer reads, as written (indexable-inc/ix, branch `explore-general`,
`packages/web/src/lib/explore/profile/`): `?profile=<url>` (section 7), `?live=<url>` (6a, with
`cursor=` on reconnect), `?open` (a picked or dropped `.ixprof`/`.ixprof.zst`), and
`?base=<url>` for a diff. It reads every table of section 3 plus `open_spans`, ignores a table
it does not know (1 says new tables keep the version), refuses `format_version` != 1 and a
container version != 1 by name, and checks each section's `rows` (not its blake3: the page has
no blake3). It requires the live answer's content type to be `application/vnd.ix.prof-stream`.

Asks, in order of weight:

1. **Check `Host` on every HTTP endpoint** (6, 6a and `blaze pprof --http`): answer 403 unless
   it is `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`. Without it a page anywhere can
   read the profile through DNS rebinding (its own origin, so CORS never applies), and the
   endpoints carry no token. The explorer's WebTransport path has a token and a pinned
   certificate for this reason; the HTTP endpoints have only loopback binding, which rebinding
   gets around.
2. **Ids that fit a double**: the page reads int64/u64 as JS numbers (as the rest of the
   explorer does). Say in 3 that `span_id` and `thread` stay below 2^53, or the page has to read
   them as BigInt.
3. **6a BATCH for `open_spans`** with zero rows: say it is still sent when the open set empties,
   so the page clears it (the daemon's feed does this with an `open` frame of `rows: 0`).
4. `alloc_events` and `counters` per tick are read; `allocs` (the aggregate) in a live stream is
   read as append-only rows. If the live stream sends `allocs` as a full replacement per tick,
   say so and the page will replace instead of append.

Unverified on the page side: that Safari lets an https page `fetch()` `http://127.0.0.1`
(Chrome and Firefox treat loopback as potentially trustworthy; Chrome asks the Local Network
Access permission first).
