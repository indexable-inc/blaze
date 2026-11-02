---
status: plan
summary: Speed of the back end's per-function side tables and settings (`crates/hir-ty-codegen/{backend,target,unwind,debuginfo}.rs`, `unwind/`, `debuginfo/`) on v2's path - the state of the art each is held to, what one function's compile cost there before and after the 2026-10-29 pass (allocations, locked instructions, shared lines written, clock reads), and what is still missing and whose it is.
date: 2026-10-29
---
# The back end's tables and settings (speed-backend-debug)

Every function v2 compiles goes through `v2/backend.rs::compiled`, which runs `crate::emit`
(`mod.rs`) and then reads four things out of these files: the function's line rows
(`LineRow::of_ranges`), its unwind information (`FunctionUnwind::of`, then `v2_parts`), where
its named slots are (`SlotPlaces::of_frame`, and `VariablePlace::pointee` from `v2/side.rs`),
and the tally of what it cost (`backend::Tally`). The Cranelift settings it is compiled with
are `target.rs`'s `Target::isa`, built once per process. v2 reaches nothing else here:
`debuginfo/{types,set,split,codeview}.rs` and `DebugContext` are v1's object writer
(`mod.rs`'s `Describer`, `cached.rs`), replaced at integration (crates.md), and were left
alone.

## The tally: per-thread counters merged on read

- **Benchmark**: the Linux kernel's per-CPU counters (`percpu_counter`): each CPU adds to a
  word of its own with no locked instruction, and a reader sums them.
- **Before**: one `static Tally` of `AtomicU64`s, `fetch_add`ed by every worker for every step
  of every function (`infer`, `mir`, `translate`, `cranelift`, and LLVM's phases): about ten
  locked read-modify-writes per function, all on the same two or three cache lines, which
  every core wrote. `Tally::passes` took a global `Mutex` per Cranelift function under
  `BLAZE_STATS`, and `alone`/`fallbacks` a global `Mutex` per reason. Cranelift's default
  pass profiler also read the clock twice and boxed a timing token for every pass of every
  function (about 20 passes), whether or not anything read the times.
- **Now** (`backend.rs`): each thread holds a `Counts` of its own, 128-byte aligned, two
  256-byte blocks of words (asserted), added to with a plain load and store: no locked
  instruction, and no line another core writes. A report sums what each word grew by since it
  last read it (`fetch_max` on a separate `reported` block, so concurrent reports split the
  growth exactly once). A thread that ends hands its counts on to the next thread that starts,
  so the list never holds more than the most threads alive at once, and nothing counted is
  lost; counts after a thread's TLS is gone go to one shared `late` block with `fetch_add`.
  `alone`, `fallbacks` and `passes` are per thread too, so their mutexes are uncontended.
  Without `BLAZE_STATS` each thread's Cranelift gets a profiler that reads no clock and
  allocates nothing (its token is `()`). Callers are unchanged (`Tally::global()`,
  `.large.fetch_add(..)`, `.step`, `.time`).
- **Matches** the benchmark. Tests: every count is its own word; eight threads add up once;
  a thread's counts are reused by the next; reports racing with counting threads see each
  count exactly once; reasons merge across threads.

## Line rows

- **Benchmark**: LLVM's `DwarfDebug` and gimli's `LineProgram` keep a row per line change
  (LLVM's `MCDwarfLineEntry` is a label, a file, a line, a column and flags; gimli's
  `LineRow` about 40 bytes), encoded as address deltas when the section is written.
- **Before**: `LineRow::of_ranges` pushed rows one at a time: about log2(rows) reallocations
  per function, capacity up to twice the rows.
- **Now**: 8 bytes a row (asserted), one allocation of exactly the row count: the ranges are
  walked twice, to count and to write. Rows are statement indices, not lines, so the row
  itself is what a debugger's line program would delta-encode; the encoding is the linker's.
  Beats both benchmarks on bytes per row. Test: same rows as the push-one-at-a-time version,
  over 300 seeded functions, with capacity equal to length.

## Slot places

- **Before**: `SlotPlaces::of_frame` collected through a filter (reallocating); `pointee` of
  a frame place built the `fbreg` expression, cloned the whole `Ranges` list (a `Vec` of
  `Vec<u8>`) of a ranged place, and then concatenated each expression with `deref` into a
  third vector.
- **Now**: `of_frame` sizes its vector from the slots once; `pointee` of a frame place writes
  `fbreg <off>; deref; plus_uconst <n>` into one vector of its exact length (LEB128 sizes
  computed), and a ranged place is read in place, not cloned. Test: against the general
  expression path over the LEB128 edges and 500 seeded offsets, with exact capacity.

## Unwind information

- **Benchmark**: Cranelift's own unwind emission (one `UnwindInfo` of instructions per
  function, turned into an FDE by gimli) and mold's `.eh_frame` handling (entries kept as the
  bytes they are, CIEs deduplicated, FDEs copied, not re-parsed).
- **Landing pads, before**: the `.gcc_except_table` entry was built as rustc's Cranelift
  backend does: a `GccExceptTable` with a `Vec` of call sites, an action table and a type
  table, written once to a counting writer and once to a growing `EndianVec`: four to six
  allocations per function with handlers, and an entry for every function with calls even
  where no call lands anywhere.
- **Now** (`unwind/lsda.rs`): every entry blaze writes has one shape (one catch-all type, one
  action, a row per call), so it is written in one pass into a buffer of its exact size, from
  Cranelift's call sites directly (12-byte `CallEntry`, never stored). A function none of
  whose calls has a handler gets no entry, as LLVM writes none for such a function of rustc's.
  Test: byte-for-byte against the old writer (kept as the test model) over 500 seeded tables,
  both address sizes, and every padding the header can need.
- **Windows, before**: `seh::function` collected the guarded source ranges into a `Vec`, then
  searched all of them for each call: O(calls x ranges) per function.
- **Now**: one merge walk over the sorted ranges and the calls in order (`Ranges`), no
  allocation. Test: against a full search over 200 seeded functions, with backward jumps.
- **Still missing**: `v2/unwind.rs::convert` (the adapter's) writes a CIE and a whole
  `.eh_frame` through gimli and parses it back for every function, and copies the CIE bytes,
  which are the same for every function of the ISA. Filed: `v2/requests/codegen-adapter.md`,
  "From speed-backend-debug" 2 (compute the CIE half once per process; keep only the FDE).
  That closes the gap to mold's model.

## Cranelift's settings (`target.rs`)

- `enable_verifier`: Cranelift's default is on, a walk of the whole function and its
  dominator tree before any code is made. Now on only under `cfg(test)` or `BLAZE_VERIFY=1`;
  the bytes are the same either way.
- `opt_level`: `none` for every Cranelift unit; at `none` the e-graph pass, and with it
  `enable_alias_analysis`, never runs, so there is nothing more to turn off. Optimizing code no
  edit reaches (std's non-generic code, devspeed's requests 8 and 14) needs a level in
  `blaze_codegen::Backend::Cranelift` so it is in the key: filed with the lead
  (`v2/requests/codegen.md`, "From speed-backend-debug" 2).
- `regalloc_algorithm`: backtracking, and `single_pass` above `mod.rs`'s `SINGLE_PASS` (50 000
  instructions); `mod.rs` gives the measured trade-off. Left as it is.
- **Target features**: x86-64's baseline is SSE2, as rustc's `x86-64` CPU is (tested).
  `aarch64-apple-darwin` now gets `has_lse`, which rustc's `apple-m1` CPU has: an atomic
  read-modify-write is one `ldaddal`/`casal` instead of an exclusive load and store loop,
  as LLVM's code of it is. The CPU is in the unit's key (`Toolchain::cpu`, compared in v2's
  `check_spec`), and blaze's code identity covers the mapping. The host's features for dev
  builds (hardware.md) need the plan to fill `Toolchain::features` and the adapter to accept
  them; filed (`v2/requests/codegen.md` 1, `v2/requests/codegen-adapter.md` 4), and the
  sibling request for an ISA per `#[target_feature]` set (`v2/requests/speed.md`,
  speed-backend-intrinsics 1) lands in the same place. The `target.rs` side (rustc feature
  names to Cranelift's `has_*` settings) is written when one of those has a caller.
