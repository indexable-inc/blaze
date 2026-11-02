---
status: plan
summary: lane safety's speed of light: the bound each hir-ty-ub component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane safety: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md).
Each component of `hir-ty-ub` has a section here, written by the agent that owns it. Cycle
counts are for one core of the target machines (Graviton 5, EPYC Turin): about 3.5 GHz, an
L1 hit in 4 to 5 cycles, L2 in about 14, and branchy pointer code at about 2 instructions per
cycle. Figures in ns use 3.5 GHz.

## The checker: `check.rs`, `check/` (safety-check)

The checker does no work of its own beyond its hooks: it is called by the evaluator once per
event and hands each event to memory, Tree Borrows and the race detector. So its bound is
per hook, and its own part of each hook should be a few instructions on lines already in L1.
The whole run is then bound by the evaluator's events times these costs. The benchmark is Miri
on the same programs, which does the same checks; `proof` compares the two once things
compile.

### `step`, once per MIR statement and terminator

Work: turn the evaluator's `Site` (body and MIR span) into the dense `SiteId` that the race
detector and Tree Borrows store with each access.

- Bound: compare the body with the current one, pick the span kind's column, check the
  index against the column's length, and load the id. That is about 16 instructions on two
  lines in L1: 8 cycles, 2.3 ns.
- Design: `Sites` keeps one table per body, found through an `FxHashMap` only when the body
  changes (a call or return), and one dense `Vec<SiteId>` per span kind indexed by the span's
  arena index. A statement is interned with no hashing. The first version hashed every
  statement's site, which costs about 30 to 40 cycles from hashing a 16- to 24-byte key,
  probing and comparing it.
- What stands in the way of the last factor: the evaluator passes the site by value. If
  `Site` carried a body-local statement index, `step` would be one indexed load, about 3
  cycles. That is a change to the `Machine` contract (codegen-mir) and is worth making only
  if `proof` measures `step` above 10% of a statement's total cost.
- Tree Borrows' garbage collection runs from `step` every 10,000 retags (Miri's default).
  It visits every live tag once, so it costs a few cycles per tag per 10,000 retags,
  amortized.

### An access (`read`, `write`, `copy`)

Work: memory's bounds, liveness and alignment check; then the race check; then Tree Borrows'
permission check.

- Memory: one allocation record (base, size, alignment, live) by `AllocId` index, one line
  and about 12 instructions.
- Race detector, one thread: `race.rs` skips everything until a second thread is spawned
  (Miri's `multi_threaded`), about 2 instructions. Several threads: one epoch compare per
  location in the common case, about 10 instructions and a line.
- Tree Borrows: the tag's node and the location's permission, one or two lines, about 20
  instructions when the access changes nothing (Miri's fast path).
- The checker's glue: about 10 instructions, no allocation, no hashing.
- Bound: about 45 instructions on 4 lines in L1 or L2, about 25 cycles (7 ns) per access,
  plus about 5 cycles with several threads.
- Design: `place`, `plain_access` and `atomic_access` call each part once, in Miri's order,
  and return at the first error. The error path is `#[cold]`. The read's shadow is a
  `memory::Bytes`, which holds values of up to 64 bytes with one provenance run inline, so
  a scalar or pointer read allocates nothing.
- What stands in the way: a read of more than 64 bytes builds a heap `Bytes` (memory's
  representation and the `Machine` trait's owned `Shadow`). A borrowed view of memory's
  shadow would remove that allocation. It is proportional to the bytes moved, so it matters
  only for large copies, and `copy` (the common way large values move) already works on
  memory without building a shadow.

### `typed`, once per typed copy (validity and retags)

Work: check that the N bytes copied are a valid value of their type, retag each of the P
references in them, and reset padding and provenance as Miri's `validity.rs` does.

- Bound: one lookup of the type's plan, then about 5 cycles per plan op (K ops), one cycle
  per 64 bytes for the init check, and one Tree Borrows retag per reference (about 50
  cycles plus the pointee's locations). That is about 10 + 5K + N/64 + 50P cycles.
- Design: a type's plan is built once per run (`Plans`, reused across schedules) as a flat
  list of ops, with enum, variant, pointee and `UnsafeCell` tables as columns. The plan is
  found with one `FxHashMap` probe on the interned `Ty`, which is a pointer. The walk reuses
  scratch vectors, so a copy allocates nothing. Strings are built only for an error.
- What stands in the way: none known in the checker. Validating every typed copy is what
  Miri does, and the checks are what make it Miri's semantics.

### `run_test`, a test under several schedules

Work: S schedules of a test, each a full run of time T.

- Bound: with C cores and many tests, total work over cores, Σ S·T / C. The critical path is
  the longest single schedule, max T, because schedules are independent.
- Design: `run_test` runs the schedules of one test in a loop, reusing the type plans, and
  stops at the first that fails. A runner reaches the critical-path bound by giving each
  seed its own job: `run_test(db, test, &Config { seed: s + i, schedules: 1, .. })`. A job
  then rebuilds the plans of the types it uses, a few microseconds each.
- What stands in the way: within one `run_test` call, schedules run one after another,
  because `&dyn HirDatabase` can't be shared across threads. With one test and many
  schedules, splitting the seeds is the runner's job (`blaze test --ub`, lane cargo), and
  the call above is how it does it.

### Memory by lifetime ([memory.md](../../design/memory.md))

The checker's warm loop is the hooks above, and they make no allocation once an allocation's
columns, a type's plan and a site exist (`tests/ub/budget.rs`). What it does allocate, by
lifetime:

- **A schedule** (one `Checker`): the per-allocation columns (`made_at`, `freed_at`), the
  runnable list, and the parts' tables. These grow to the run's high-water mark and are
  dropped with the checker. They belong in the request arena of the test run, with its own
  tag (`ub`).
- **A test** (one `run_test`): `Plans` and its scratch buffers, kept across the test's
  schedules. They also belong in the test run's request arena. They can't outlive the
  database revision, because they hold `Ty<'db>`.
- **A failure:** the report's strings and the `Outcome`. These come from the general-purpose
  allocator, once per failing test, at the edge.

When `blaze-mem` lands, these columns move onto its arena and pool types. Until then they
are `Vec`s grown by doubling, which allocate O(log n) times per run.

### What `proof` should measure

- ns per `step`, per access, and per typed copy against the bounds above;
- the checker's run time over the plain evaluator's (`Plain`) on the same program;
- the checker against Miri on the same programs.

A component more than 10% off its bound is a bug.

## The checker's parts: memory, Tree Borrows, races, validity (speed-sol2-proof)

The checker's section above bounds each hook as a whole. This one counts each part's share of
it from the code. The benchmark for every part is Miri's own algorithm, since these are ports of
it, at the same cache lines. The crate was held by lane unify-front while this was written
(`STATUS: IN`), so nothing here changed the code. The one gap over 10% is queue item 00898.

### Memory (`memory.rs`, `memory/bytes.rs`, `init_mask.rs`, `prov_map.rs`)

- **Access with concrete provenance** (`access_as`): the `AllocId` indexes `infos` (an `Info`
  is 32 bytes, half a line, `const`-asserted). Then one live, one mutability and one bounds
  check (`check_bounds`, two adds and a compare), and the alignment mask. That is about 15
  instructions on one line, the bound, and the address map is not touched.
- **Wildcard provenance and `live_at`/`exposed_at`**: a binary search of `live`, sorted by base:
  log₂ N probes, the bound for a sorted array.
- **`alloc` and `free`**: the same search, then `Vec::insert` or `Vec::remove` into `live`. At an
  address above every live one (the evaluator's bump order) that is O(1). At any other it
  shifts the tail: 16 bytes for each live allocation above it. With N = 10⁴ live heap blocks
  freed oldest first that is about 80 KB moved, about 3 µs a free, **about 30× the O(log N)
  bound** (Miri's `int_to_ptr_map` costs the same). The fix is a `BTreeMap<Addr, AllocId>`, a
  range query for the lookups and O(log N) for insert and remove: queue 00898, with the exact
  edit and the model test it needs.
- **Read of a value** (`read`): the init mask's lazy state (all initialized, or all not) answers
  with no bit copy. Otherwise one word of mask per 64 bytes, copied by `bits`. Provenance is
  one merge of the pointer map's and the fragments' runs in the span, each a binary search then
  a walk. Values up to 64 bytes with one provenance run stay inline (`SmallVec<[u64; 1]>`,
  `SmallVec<[Run; 1]>`). Bound: N/64 words plus the runs. A read over 64 bytes allocates; see
  the checker's access section for what stands in the way.
- **Write and copy**: one `set_range` or `set_bits` on the mask (a word per 64 bytes), one
  `clear` of the provenance range, and inserts of presorted runs, all through scratch vectors
  reused across calls. Bound: the same, and today's code is there.

### Tree Borrows (`borrows.rs`, `borrows/*`)

- **Access**: `Tree::perform_access` walks only the locations of the accessed range
  (`RangeMap::iter_mut`, a binary search then the runs). For each run it visits the tree from the
  accessed tag with Miri's foreign-access skipping (`skip_if_known_noop`, `IdempotentForeignAccess`),
  so a foreign subtree already in the state the access would leave is not entered. A
  `LocationState` is 1 byte with its `Option` (`const`-asserted).
  - Bound: the nodes whose state the access changes, plus one per skipped subtree root. Today's
    code is at it.
- **A protector's end**: every location of the allocation, as Miri does, because the access
  is to whatever the tag accessed. Bound: the locations, times the nodes the access changes.
- **Retag**: one node pushed (`children: Vec<SmallVec<[UniIndex; 4]>>`, inline up to 4
  children), the tag's index found through `UniKeyMap` (one `FxHashMap` probe; the tags are
  sequential `u64`s, trusted). Then one location walk of the pointee's range.
- **Garbage collection**: every 10,000 retags, a visit of every live tag. That is a few cycles
  a tag, amortized, as the checker's section says.

### The race detector (`race.rs`, `race/*`)

- **One thread**: every hook returns at the `multi_threaded` test (`race.rs` 298, 377, 719,
  963, 979), about 2 instructions, which is the bound. `Slot::Live` clocks are not even made.
- **Several threads**: a location's clocks are found through `race::range_map` (a binary
  search). A plain access compares one epoch (`VTimestamp`, 8 bytes, `const`-asserted) in the
  common case. A join is one pass over the thread count (`VClock`, a `SmallVec` inline up to
  `SMALL_VECTOR` threads). Atomic cells are boxed only once a location is used atomically
  (`Option<Box<AtomicMemoryCellClocks>>`). Bound: Miri's FastTrack epochs. Today's code is at it.
- **Weak memory** (`weak.rs`): a store buffer per atomic location, with loads keeping their
  timestamps inline up to 4 (`SmallVec<[_; 4]>`), as Miri does.

### Validity (`check/valid.rs`, `check/offset.rs`)

The plan walk the checker's `typed` section bounds: one `FxHashMap` probe on the interned type,
then flat ops. The ops read the init mask a word per 64 bytes and retag each reference once.
Today's code is at that bound.

### What `proof` should measure

- ns per `free` with 10⁴ live allocations freed oldest first, against ns per `free` of the
  newest. These differ by the tail move until 00898 lands.
- ns per access with one thread against the plain evaluator (`Plain`), which shows the race
  detector's and Tree Borrows' fast paths.

## The proc-macro wire's fuzz targets: `fuzz/targets/proc_macro_*` (safety-fuzz-procmacro)

These targets are not part of `hir-ty-ub`, but they are safety's code, so their bounds go here.
A fuzz target's speed is runs per second per core. The engine runs one process per core with
nothing shared, so runs scale with cores. The bound for one run is the work its invariants
need: the reads, writes and comparisons each check names, at the reader's own cost. Anything
the target adds beyond that is overhead to remove. Costs are per input byte (n bytes), from
the per-op figures at the top:

- **Postcard decode:** about 4 cycles a byte. Most of a message is `u32` varints of 1 to 5
  bytes at about 5 cycles each, and COBS decoding plus `read_until`'s `memchr` add about 1.
- **Postcard and COBS encode** (`Message::write`): about 3 cycles a byte.
- **JSON** (legacy) decode or encode: about 5 cycles a byte.
- **`Debug` formatting:** about 50 cycles per `u32` through `fmt::Formatter`, so 15 to 50
  cycles a byte, which is 5 to 15 times a decode.
- **A one-byte-at-a-time read:** a `fill_buf`, `memchr`, `extend` and `consume` through
  `&mut dyn BufRead` per byte, about 60 cycles a byte on top of the decode. That is about 15
  times a whole read. Two-byte pieces cost about half that, and the uneven cut about a third.

The engine's own cost per run (mutation, resetting and comparing the coverage map, the 2 to 3
times slowdown of instrumented code) is blaze-fuzz's (safety-fuzz) and is the same for every
target, so it is not counted here.

### `proc_macro_message`: one postcard message

- Work: frame the input (1), read (4), write (3), read the frame back (4), and compare the two
  messages. The messages have no `PartialEq`, so the comparison is a second write (3) and a
  `memcmp` of the two frames. That makes 15n.
- Bound: with `PartialEq` on the messages, the comparison is about 0.5n, for 12.5n. The target
  is at 83% of that.
- Before this change: both messages were compared by `Debug` (`canonical`), about 2 × 25n, for
  62n in all, 20% of the bound.
- Design: compare the written frames, and use `canonical` only when they differ. That happens
  only when a hash table's entries come out in another order after a round trip (colliding or
  duplicate keys), which is rare.
- Corrected by speed-sol2-proof-fuzz: the messages now derive `PartialEq, Eq`. But
  `Message::write` takes the message by value and the messages are not `Clone`, so the message
  read first is gone after the round trip. The cheapest comparison is still the second write
  (3n) and a frame compare (0.1n); a `clone` and `==` would be about 2.5n. That makes 15.1n
  against a bound of 14.5n, within 5%. `says` (two reads) runs only when the frames differ.

### `proc_macro_frames`: a stream of frames, whole and in pieces

- Work: read the stream whole (4n) and frame by frame as the reference (4n), read it once in
  pieces (4n plus the cut's overhead, which averages about 25n over the five cuts), and write
  every message of the three reads to compare them (9n). That makes 46n.
- Bound: with `PartialEq`, the writes become about 1n, for 38n. The target is at 83%.
- Before this change: every run read through all five cuts (about 144n in all) and compared
  seven reads by `Debug` (about 175n), for about 327n, 12% of the bound.
- Design: one cut per run (`shared::sizes`), picked by the input's length and last byte. The
  engine's mutations reach every cut over many runs, at one fifth of the cost. All three reads
  are of the same frame bytes, so they decode to the same message with its hash tables in the
  same order, and comparing written frames is exact here.
- Corrected by speed-sol2-proof-fuzz: the target compares its three reads with `==` now, 4n +
  4n + 4n plus the cut's 25n plus about 1n, for 38n, the bound. Optionally, a public
  frame-level reader (`transport::postcard::read` is `pub(crate)`) would let the pieces read
  check framing alone, with no decode: 23n would become about 19n.

### `proc_macro_legacy`: lines of JSON

- Work: read whole and write each answer (10n), read once in pieces and write (5n, plus the
  cut's overhead of about 25n, plus 5n), then read each written line back and write it again
  (10n). That makes 55n.
- Bound: with `PartialEq`, one write per answer remains (the round trip needs it), and the
  others become comparisons, for about 46n. The target is at 84%.
- Before this change: four cuts per run, each read and written, about 173n in all, 27% of the
  bound.
- Corrected by speed-sol2-proof-fuzz: it compares with `==` now and writes each answer once to
  read it back: 10n + 30n + 5n + 1n, about 46n, the bound.

### `proc_macro_flat_tree`: the token-tree encoding inside an answer

- Work: decode (4n, plus `to_subtree_resolved` at about 30 cycles a token, about 5n), encode
  (`from_subtree` with a hash-set insert per span, about 8n, plus 3n to write), decode again
  (9n), and compare the trees with tt's columnar `PartialEq` (about 1n). That makes 30n, which
  is the bound: each of these steps is the round trip itself.
- Before this change, `reshaped`, which collects every token into a `Vec` (about 3n), ran on
  every run. It now runs only when the trees differ, so the target is at its bound.

### `proc_macro_tt_wire`: `TopSubtree::read_wire`

- Work: read (about 2 cycles a byte for the checks, plus about 75 cycles per symbol to intern
  it, about 3n), write (0.5n, mostly copying columns), compare the bytes (0.1n), rebuild with
  `from_serialized` (a span-part hash lookup per token plus a refcount per symbol, about 6n),
  and read one strict prefix (at most 3n, 1.5n on average). That makes about 12.6n, the bound.
- Design: one prefix per run, its length set by the byte after the tree, or all but the last
  byte when nothing follows. The first version read four prefixes, about 2.5 reads (7.5n),
  for 17.1n.

### Memory by lifetime ([memory.md](../../design/memory.md))

The targets run in the fuzz engine's process, not in blaze's warm loop, so the zero-allocation
rule doesn't bind them. They also must not move the reader's allocations off the global
allocator: `blaze_fuzz::largest_allocation` counts the reader's calls to it, and that count is
the allocation bound each target checks. By lifetime:

- **One run** (a task, in memory.md's terms): what the reader under test allocates, which is what is
  measured; the written frames or lines the check compares, one per message; and the `Vec`
  of reads, a few doublings per run. The piece sizes live in a caller's `[usize; 8]`. If the
  engine ever gives each run a `blaze-mem` frame allocator, the target's own buffers can move
  onto it, outside the measured closure. Until then they are a few allocations per message,
  next to the reader's own.
- **The process:** seeds and fixtures (`shared::trees`, `shared::messages`), built once.
- **A failure:** `canonical`'s `Debug` text and the assert messages, at the edge.

### What `proof` should measure

- Runs per second per core for each target, at its corpus's mean input length.
- Cycles per input byte against the figures above.
- The targets' own share of a run, measured by profiling the engine with a target whose check
  does nothing.

Corrected by speed-sol2-proof-fuzz: the messages derive `PartialEq` now, so `proc_macro_frames`
and `proc_macro_legacy` are at their bounds and `proc_macro_message` within 5% of its own.
`proc_macro_flat_tree` and `macro_wasm_wire` (usercode-wire's) were audited as they stand: one
decode, one encode, one decode again compared with `==`, and one host run with its setup in a
`OnceLock`. Both are at their bound.

## The fuzzer: the engine `fuzz/src/` and a fuzz job (safety-fuzz)

A fuzz run is the target's `check` on one input, plus the engine's work around it. The
engine's job is to stay a small, fixed cost per run, so that the job's rate is set by the
targets. The benchmark is libFuzzer, which does the same work per run: `ResetMaps`, then
`CollectFeatures` over the 8-bit counters, then the value profile and TORC. Its own bound
is the same as ours.

Figures are for one core of the target machines (3.5 GHz, as above). Per core, L2 delivers
64 B per cycle on both Turin and Graviton 5; L3 about half that. Stores run at 1 to 2 per
cycle, and a mispredicted branch costs about 15 cycles. L2 is 1 MiB on Turin and 2 MiB on
Graviton 5.

Symbols used below:
- N: the counters in the binary, one per instrumented block. Expect about 2^18 for a store or
  linker target and about 2^20 for the `hir_*` targets, which link hir-def and salsa.
- H: the counters a run reaches, about 10^3 to 10^4 for a parser.
- V: the value-profile bits it sets.
- L: the input's length.

### The engine's work per run

| step | work | bound (cycles) |
|---|---|---|
| clear before the run | read N bytes a line at a time; zero the D lines that hold a count | N/64 + D |
| drain after the run | read N bytes a line at a time; one feature per reached counter | N/64 + 3H |
| value profile | 1 Ki loads and 1 Ki stores (relaxed atomics: the watchdog may trace into it), an 8 KiB snapshot, one feature per bit | about 1.5 K + 3V |
| merge into the seen set | one bit test-and-set per feature, in a bitset of N + 8 KiB bytes held in L2 | 2(H + V) |
| pick and mutate | copy the parent, up to 5 mutations, each at most one move of L bytes or, for the two that replace a recent operand, one search of L bytes | 6L/32 + 100, plus about L per search |
| clock | one vDSO read per run, shared by the deadline, the watchdog stamp and the status line | about 90 |

For N = 2^18, H = 2000, V = 500 and L = 1 KiB, the bound is 4096 + 500 + 4096 + 6000 + 3000 +
5000 + 300 + 90, about 23 K cycles or 6.5 µs. The scans are 8 K cycles of that, and the
per-feature work is most of the rest.

How the code reaches each term:
- `__sanitizer_blaze_fuzzer_run` and `__sanitizer_blaze_fuzzer_drain` (`raw.rs`) test a cache
  line of eight words for zero with one OR, which compiles to two 32-byte or four 16-byte
  loads. Only the lines with a count go word by word.
- The clear writes only the dirty lines. A `memset` would store all N bytes at store
  bandwidth (half the load rate) and dirty every line, which costs a write-back of N bytes per
  run once N outgrows L2.
- Both functions are named `__sanitizer_*`. SanitizerCoverage skips such functions (LLVM's
  `SanitizerCoverage.cpp`, and blaze's backend by the same rule), so the scan carries no
  counters and no trace-cmp calls. It also can't add bits to the value profile while reading
  it. Both run with ASan off (`#[sanitize(address = "off")]`).
- The merge indexes a bitset directly, at `index << 3 | bucket`, so a counter's eight buckets
  share one byte.
- The fuzz loop reads the clock once per run.
- The run buffer is reused, even after a keep; tests hold a run that keeps nothing and a
  keep to zero general-purpose allocations.
- (speed-sol2-proof-fuzz, a correction: this term was missing.) `RecentCompare` and
  `RecentBytes` replace an operand where they find it in the input (`replace_or_put`). The
  search was a slice `==` at every offset, each a `memcmp`.
  - In a fuzz build, ASan intercepts that `memcmp` and calls the engine's own
    `__sanitizer_weak_hook_memcmp`, which copies 2 × 32 bytes, sets a value-profile bit and
    took a locked slot. That is about 70 cycles an offset, about 35L for a search that finds
    its operand halfway. A run makes about 0.46 searches, so about 16L a run: 16 K cycles at
    L = 1 KiB, against the engine's whole 23 K bound.
  - Each of those compares also wrote the engine's own operands into the words table the
    dictionary mutations read.
  - Now the search is `raw::__sanitizer_blaze_fuzzer_find`, which SanitizerCoverage skips. It
    compares a byte at a time with no `memcmp`, at about 1 to 2 cycles a byte, about 0.5L a
    run. It finds the same offset, so the mutator draws the same numbers and makes the same
    inputs; `finding_an_operand_agrees_with_a_search_at_every_offset` compares it with the
    old search on 20000 inputs.
  - Left: a SIMD `memchr` (about L/32) would need the workspace's `memchr`, a lock-file change.
    At 0.5L against a 23 K-cycle run, it isn't worth it.

### Memory by lifetime ([memory.md](../../design/memory.md))

- **The process.** The kept corpus is three columns over reserved regions
  (`blaze_mem::RegionVec`): the inputs' bytes end to end, their ends, and the running sums
  of their weights. Each column grows by committing pages and never moves, so a keep is one
  copy and two pushes, and a pick is a binary search and a slice. Reaching the reservation
  (16 GiB of bytes, 2^26 inputs) stops the job with an error naming it. The seen bitset, the
  feature list and the run buffer are sized once at setup.
- **One run.** Nothing from the general-purpose allocator. A kept input's file is written
  from scratch buffers (text and two paths) that grow once to the longest input. The set of
  kept files is keyed by content hash and reserved for 2^14 entries, so it grows only past
  that, by doubling.
- **The edge.** Reading another worker's file (once per file), the directory listing (once a
  second), a finding's report and error messages allocate, each marked `// alloc:`.
- Until blaze-mem has a fuzzer tag, the corpus counts under `Tag::Other`
  (`v2/requests/mem.md`, "From safety-fuzz").

The previous version of the engine was far from these bounds:
- Its drain made a variable-length copy per 8-byte word, about 15 cycles, or 2N cycles in
  all: 60 times the scan's bound.
- It cleared the value profile with 1 Ki atomic swaps. Each is a locked `xchg` on x86, about
  20 K cycles in all.
- It cleared the counters with a full `memset` before every run.
- The drain was instrumented, so every comparison in it called a trace-cmp callback.

**Left between the code and the bound:**
- One mispredicted branch per run of nonzero lines in each scan, about 15 cycles each. With
  coverage in a few hundred clusters, that is a few thousand cycles per run.
  - If `proof` measures it above 10%, the fix is branch-free: append each line's index and
    add one when it is nonzero (`lines[n] = w; n += (any != 0) as usize`), then walk that
    list. The clear does the same over the indexes the drain found, plus the lines the
    engine's own code reached between runs.
  - Those lines are what keeps the clear a full scan. The mutator, the corpus pick and std
    (`Instant::now`, `Vec`) are instrumented, and they run between the drain and the next
    run. Making all of them uninstrumented would let the clear skip its scan and write only
    the D lines the drain listed, halving the scans.
- N itself. Every term above with N in it is time spent on code the input never reached, and
  a counter of a function the link dropped should not be there at all. That is requested of
  link (`v2/requests/link.md`, "From safety-fuzz"): drop SanitizerCoverage's entries with
  their function, as lld and ld64 do. Until then, a `hir_*` target's N may pass L2, and its
  scans run from L3 at half the rate.

### The comparison callbacks, once per comparison the target makes

Work per traced comparison:
- the return address;
- the operands' Hamming distance (`xor`, `popcnt`) and leading zeros (`sub`, `lzcnt`);
- two value-profile bits (each a mask, a shift, a load, an `or` and a store, in 8 KiB that
  stays in L1);
- one TORC slot (load and bump the index, three stores).

That is about 20 instructions, roughly 7 cycles at 3 per cycle, and the code is now exactly
that. The earlier version spent two locked `or`s and a locked `xadd` on each comparison:
- on x86, each locked instruction is a full barrier that drains the store buffer, about 20
  cycles;
- on Graviton, LSE atomics may execute far, in the interconnect;
- either way, 40 to 60 or more cycles per comparison, 6 to 8 times the bound, on the hot
  path of every target.

Corrected by speed-sol2-proof-fuzz: the `memcmp` hook (`__sanitizer_weak_hook_memcmp`) still
took its words slot with a locked `fetch_add`, about 20 cycles on every differing `memcmp` the
target made. It now takes the slot as `recent` does, with a relaxed load and store. The relaxed
loads and stores can lose a bit, or a slot, that two of the target's threads write at once,
which libFuzzer's maps accept too. With this, no callback has a locked instruction. The test
`a_byte_comparison_that_differs_is_kept_for_the_dictionary` calls the hook and finds the
operands in a slot.

### A fuzz job: the rate of runs

- Bound: C cores give C / (T_check + T_engine) runs per second.
- One process uses one core, because a process's counters are its own. On a 16-vCPU CI
  runner that was a sixteenth of the bound.
- `fuzz --jobs N` runs N worker processes with seeds `seed + k`. They share the kept corpus
  through `out/<target>/corpus/`.
- Each worker reads the directory once a second: one listing with no `stat` per file, and
  only new names are read. Files appear whole, written under a `.` name and renamed.
- The parent polls its workers every 100 ms.
- Both costs are below 0.01% of a core, so N workers give at least 99% of N times one
  worker's rate.
- What remains is search overlap between workers, which is the same as with libFuzzer's
  `-jobs`, and shared L3 bandwidth once N passes L2. The second is covered by the link request
  above.
- CI is asked to run `--jobs "$(nproc)"`, and to build the 52 binaries in one build rather
  than 52 (`v2/requests/safety.md`, F1 amended).

### The plain `#[test]` of each target

- Work: 20000 changed inputs plus the corpus, each one `check` without coverage.
- Bound: the total over all cores. It used to run on one thread; `seeds` now hands out inputs
  through one shared atomic index on every core, and the first failure stops the rest.
- `Target::check` must allow calls on several threads at once. Every target does: the shared
  ones (`macro_wasm_*`) take their own lock, and `blaze_fuzz`'s allocation counter is per
  thread.
- (speed-sol2-proof-fuzz) A target whose invariants name several ways of reading an input
  checks one way a run (`blaze_fuzzer::case`, a pure function of the input's length and last
  byte). Its second test, `every_case` (`blaze_fuzzer::every_case`), runs every way of each
  corpus file and built seed. On those inputs that is the check as it was before, so it is the
  differential test of the cheaper check. It runs on every core with the same shared index.

### Minimizing a failure

- Work per try: the target's `check`, plus handing over the input.
- Before, each try was a new process (`run`, about 2 to 5 ms with ASan's start-up), a file
  write, and a 1 ms polling sleep.
- Now `minimize` keeps one `serve` child and sends each try over a pipe, so a passing try
  costs one round trip, about 10 µs plus `check`.
- A failing try replaces the child, because a panic may leave state behind, such as a
  poisoned lock. Only failing tries pay for a process.
- A timeout is the child's own timer exiting with code 3, so the parent never polls.
- Bound: P passing tries times (T_check + 10 µs), plus F failing tries times the process
  start-up. ddmin needs about as many passing tries as failing ones, so this is within 2 times
  the in-process bound.
  - Getting the failing tries into the process too would need forking a warmed child per
    failure, and fork is unsound with the watchdog thread running. It isn't done, because
    minimizing runs only after a finding and is budgeted at 300 s.

### What `proof` should measure

- The engine's cycles per run with a `check` that does nothing, against
  `2N/64 + D + 5H + 5V + 1.5K`, on a small target (`cas_head`) and a large one (`hir_body`).
- Cycles per traced comparison: a `check` that runs 10^6 comparisons, against an
  uninstrumented build, over 10^6.
- Runs per second with `--jobs C` against C times one worker's rate.
- The plain test's wall time against its single-threaded time over the core count.

A component more than 10% off its bound is a bug.

## `fuzz/src/mutate.rs`: the mutator (speed-sol2-proof-fuzz)

- Work per run:
  - pick a depth of 1 to 5;
  - per mutation, a uniform pick of the 13 and up to 16 tries until one applies;
  - each mutation is O(1) bookkeeping plus at most one `copy_within`, `splice` or `truncate`
    of up to L bytes (`EraseBytes`, `InsertByte`, `InsertRepeatedBytes`, `CopyPart`,
    `CrossOver`, `Dictionary`'s insert);
  - or a fixed few bytes (`ChangeByte`, `ChangeBit`, `ShuffleBytes` of at most 8,
    `ChangeBinaryInteger`).
  - The others:
    - `ChangeAsciiInteger` scans for a digit run, at most L byte loads, and formats at most
      20 digits.
    - `RecentCompare` and `RecentBytes` search for the operand (above).
- Bound: one move of L bytes per mutation is 6L/32 for 5 at memmove speed, plus one scan
  where the mutation needs one (a search, a digit run), about L each, plus about 20 cycles of
  RNG and bookkeeping per try.
- Today:
  - Search: about 35L for each of the 0.46 searches a run, about 16L, was the only term over
    its bound. Now about 1 to 2 cycles a byte.
  - Digit scan: `ascii_integer`'s is instrumented (std's `position`), about 10 cycles a byte,
    in 1 of 13 mutations. That is about 2.3L a run at three mutations: within the "plus
    about L per scan" term at L up to a few hundred bytes, about 2 times over it at 1 KiB.
- Gap and how it closes:
  - Search: closed (the correction above).
  - Digit scan: moving it into `raw.rs` under a `__sanitizer_*` name, as the search was,
    would make it uninstrumented. Left for `proof` to measure: at L = 1 KiB it is about 2.3 K
    of a 23 K-cycle run, about 10%.
- Determinism: the search gives the same offsets, so the same seed gives the same inputs
  (`mutations_stay_in_the_limit_and_repeat_with_the_seed`).
- What `proof` should measure:
  - cycles of `mutate` per run over a 1 KiB corpus, with `RecentBytes` forced, before and
    after;
  - `ascii_integer`'s share.

## `fuzz/src/lib.rs` beyond the engine: `Target`, the plain tests, `serve` (speed-sol2-proof-fuzz)

- `main`'s dispatch, option parsing and `--jobs` set-up run once a process: not per run.
- `case(data, cases)`: one add and one modulo per run.
- `seeds(target)` (the plain test):
  - Work: collect the corpus, the seeds and 20000 changes (one copy each, at setup), then one
    `check` each on every core through one relaxed `fetch_add` per input.
  - Bound: the checks. The setup is 20000 copies of inputs of at most a few KiB, below 1% of
    the checks.
- `every_case(target, cases, check_case)`:
  - Work: the corpus and the seeds, each through `check_case` at every case, `inputs × cases`
    runs through one shared index on every core.
  - Bound: those runs.
- The fuzz loop, per run:
  - one `Instant::now`;
  - `data.clear()` and one copy of the parent (L);
  - a second pick for `CrossOver`'s other (a slice, no copy).
  - This is the "pick and mutate" row; at its bound.
- `serve` (minimizing):
  - Work per try: read a `u32` length, `resize` the one buffer (no allocation once it holds
    the longest), `read_exact` the bytes (one copy over the pipe), run, write one byte back
    and flush.
  - Bound: one pipe round trip plus the copy. At its bound.
- Nothing to close.

## The `api_*` targets: blaze-protocol and blaze-api (speed-sol2-proof-fuzz)

At their bound, unchanged:

- `api_cbor_encode`: the shape tree it builds is the input's.
- `api_envelope`: each named read is an invariant of its own.

The other five checked several ways of reading the same input on every run. Each now takes
one way a run, and its `every_case` test takes every way.

| target | work a run before | after (the bound) | the ways |
|---|---|---|---|
| `api_cbor` | check 1.5n, two decodes 13n, 7 typed reads × 2 decoders (each about 2n plus a formatted error) 28n + 7 K, three encodes 9n: about 51.5n + 7 K | 1.5n + 13n + one typed pair (4n + 0.5 K) + 9n = 27.5n + 0.5 K | 7 typed reads |
| `api_frame` | walk 0.5n, `read_frame` and `read_frame_into` each whole (1n) and through `Trickle` (8n) = 18n, `receive` and `from_slice` 10n, write 1n: about 30n | 0.5n + 4.5n average + 10n + 1n = 16n | 4 readers |
| `api_frames` | walk 0.5n, whole 1n, three cuts (bytewise 40n, uneven 10n, `[4096, 7]` 1.5n): about 53n | 0.5n + 1n + 17n average = 18.5n | 3 cuts |
| `api_http_request` | 2 body bounds × (whole 5n, bytewise about 60n, each request written and parsed again about 5n): about 140n | 70n | 2 bounds |
| `api_http_response` | 2 body bounds × (whole 5n, bytewise about 60n, `read_response` 6n): about 142n | 71n | 2 bounds |

At n = 200, `api_cbor` goes from about 17.3 K cycles a run to about 6 K. The 7 typed reads were
over half its run, because most fail at the first byte with a message `custom` formats.

## The `cas_*` targets: blaze-cas (speed-sol2-proof-fuzz)

- `cas_cold`, `cas_entry`, `cas_head`, `cas_index` and `cas_pack`:
  - Work: build the store files the input describes, write or damage them, read them back.
    The write is part of the round trip each invariant names, so it is the bound.
  - The per-byte CRC (`crc32c` a byte at a time in `cas_shared`) and the pack copies are
    under 5% next to the store's own hashing.
  - At their bound.
- `cas_remote`, mode 1 (`serve`, a third of runs):
  - Before: wrote a store of `served()` on every run with `shared::write`. That is:
    - three `put`s;
    - a flush driven through the simulation, packing and hashing about 2.2 KB, with its
      index and head;
    - then opened it.
    - Estimate: 20 to 40 K cycles before the first ask, against asks a few hundred bytes long
      at about 10 to 20 cycles a byte.
  - Now: the store's files are written once a process (`served_files`, a `OnceLock` of each
    file's name and bytes, from `shared::files` and `Sim::contents`). Each run copies them
    into a fresh simulation (about 2.5 KB and three inserts, about 1 K cycles) and opens the
    store as before.
  - Why that is the same store:
    - `blaze-cas` reads no file times (nothing in `crates/blaze-cas/src` reads
      `modified_ns`);
    - it writes every file directly under its directory;
    - the other cas targets already open stores from `shared::store_of`, a simulation
      holding file bytes and nothing else.
  - The `Store` itself is not held across runs: it carries epochs and limbo, and an input's
    coverage must not depend on the runs before it.
  - Modes 0 and 2 build what the input says: at their bound.
- What `proof` should measure: `cas_remote` mode-1 runs per second, before and after.

## `codegen_*`, `hir_*`, `intern_table`, `live_*`, `macro_wasm_*` (speed-sol2-proof-fuzz)

All at their bound; nothing changed. One shape: setup that doesn't depend on the input is in
a `OnceLock`, and each run reads, writes or compares what its invariants name, once.

- `codegen_asm`: one assemble or decode and its comparison.
- `codegen_blob`: `owned()`, the one copy it makes, is about 6% of the run.
- `hir_*` (with `hir_shared`, `hir_expr_shared`): the valid buffers are built once a
  process.
- `intern_table`: one restore into a fresh table and one write.
- `live_message`: one view, one decode, one encode each.
- `live_patch`:
  - one decode and encode, then `fuzz_checks` at three slides;
  - the slides are the one candidate for one-a-run here, but `fuzz_checks` is the checks
    before anything is mapped, small next to the decode;
  - left for `proof` to measure: over 10% of the run, it takes one slide a run like the
    others.
- `macro_wasm_host`, `macro_wasm_lex`, `macro_wasm_validate`: the host and module in a
  `OnceLock`, one run each.

## The `link_*` targets: blaze-link's readers (speed-sol2-proof-fuzz)

- Work per `blaze_link::fuzz_read`: build an `Output` and a `model::writer` (S), then the read
  (R).
- Before, per run:
  - The 7 two-arch targets (`link_elf_*`, `link_macho_archive`, `link_macho_dylib`,
    `link_macho_object`, and `link_macho_tbd`'s reads) read the input for both arches, each
    aligned, shifted and with the other kind or padded: 4 to 6 calls, (4 to 6)(S + R).
  - `link_coff_archive`, `link_coff_import` and `link_wasm_archive`: 3 calls.
  - `link_coff_object`: 2 or 3 calls.
  - `link_macho_tbd`: 6 reads plus 6 text differentials (2 arches × 3 platforms).
  - `link_macho_trie`: 4 dylibs built and read, plus 2 shifted reads, plus the trie
    differential.
- Bound: the aligned read, with its allocation bound, plus one compared read. Each compared
  read is an invariant of the aligned one, and the arch and platform are ways of reading the
  same bytes. That is 2(S + R) a run, or 1 when the padded read's case meets a refused input.
- Now: one case a run picks the arch and the compared read (and for `link_macho_tbd` the
  platform), so every combination is some case. Each target makes only the copy its case
  needs.
  - Two-arch targets: 2 calls, from 4 to 6.
  - Single-arch targets: 2, from 3.
  - `link_macho_tbd`: 2 reads and 1 differential, from 6 and 6.
  - `link_macho_trie`: the differential plus 2 dylib reads (plus a shifted one in 2 of 3
    cases), from 6.
- `link_wasm_object` (one arch, one compared read) was at its bound and is unchanged.
- Left: S is blaze-link's, paid per read. Queue 00922 (link) asks to measure it on a 256-byte
  refused input and, if it is over 10%, to give the `fuzz` feature a reader made once per
  (format, arch).
- What `proof` should measure:
  - runs per second of `link_elf_shared` and `link_macho_tbd`, before and after;
  - S against R on a small input.

## The `syntax_*` targets: parser and syntax (speed-sol2-proof-fuzz)

- `syntax_reparse` (`CheckReparse::run`: a parse, a reparse, a full parse to compare): each
  step is the invariant. At its bound.
- `syntax_parse`:
  - Work:
    - lex (Lf);
    - `LexedStr::lexes_alike`, which lexes fast again (Lf) and with `rustc_lexer` alone (Lr);
    - parse (P), the bodies, the walks;
    - in `Edition::CURRENT` (one edition of four), `syntax::fuzz::check_parser`, which
      parses again (P) only to run the private `check_file_invariants`.
  - Bound: Lf + Lr + P + bodies + walks + the block check.
  - Today: one Lf and a quarter of a P over that. With Lf about P/4, that is about 0.5P over
    about 2.25P, about 22%.
  - Both extras are in crates outside `fuzz/`:
    - queue 00920 (format) asks for `syntax::fuzz::check_parse(&Parse<SourceFile>)`;
    - queue 00921 (format) asks for a `LexedStr` method that compares an existing lex with
      `rustc_lexer`'s.
  - When they land, `syntax_parse` calls them on the lex and the parse it has.
- What `proof` should measure: `syntax_parse` runs per second before and after those two.

## The `usercode_*` targets: what user code writes (speed-sol2-proof-fuzz)

At their bound, unchanged:

- `usercode_index_shape`, `usercode_run_value` and `usercode_trace_report`: one read and its
  own checks.
- `usercode_script_verify`: one `verify`, and its model, which formats the expected message
  only when the two differ, as `verify` must.
- `usercode_trace_record`: one whole read and one read as the record grows (three pieces,
  one read in all), which is the invariant.

Each of the other three now takes one way a run (`blaze_fuzzer::case`) and has an
`every_case` test:

| target | before | after (the bound) | the ways |
|---|---|---|---|
| `usercode_wire` | walk 3n, whole 4n, five or six cuts (bytewise 44n, two by `needed` 8n, uneven 14n, `[4096, 7]` 4n, the input's own 6n), one decode 3n: about 86n | 3n + 4n + about 13n + 3n = 23n | 5 cuts, plus the input's own when it has sizes |
| `usercode_script_output` | 3 contexts × (parse whole, parse its directives alone, parse each directive alone: 3P), plus the three `Case`s built every run (about 25 allocations, 2.5 K cycles): about 90n + 2.5 K at P about 10 cycles a byte | 3P = 30n, the `Case`s built once (`OnceLock`), one `Option<String>` cloned into the context | 3 contexts |
| `usercode_script_elide` | lex and check the tokens (L), elide (E), then elide again with another file (E plus a copy), a byte order mark (E), `\r\n` (E plus 2n) and a token after `main` (E): L + 5E, about 7.5L | L + E + one change's E, about 3.6L | 4 changes |

## The `vfs_*` targets: blaze-vfs (speed-sol2-proof-fuzz)

All at their bound; nothing changed for speed.

- `vfs_journal_bytes` and `vfs_journal_records`:
  - two recoveries (the invariant is that the same bytes recover the same);
  - one walk of each head (a `format!` per path, beside four VFS calls per path);
  - one move and settle;
  - one recovery of the settled file.
- `vfs_journal_model`:
  - the transactions the input describes, with a three-name heads snapshot each (under 10%
    of a commit);
  - one recovery after the crash and one after settling.
- `vfs_image`: the image of the tree the input describes, its sections copied once to
  damage, rebuilt only when damaged, one restore, and the walks it compares.

Queue 00878 (blaze-vfs's journal format moved to BZJ4), done:

- `vfs_journal_records.rs`:
  - `MAGIC` is `BZJ4`;
  - `CHECKSUM_DOMAIN` is `"blaze v2 vfs journal record 2"`;
  - `frame` sums a file record over `ContentId::of(FILE_DOMAIN, payload)`, its payload
    otherwise, as `journal.rs`'s `frame_file` does.
- The three dictionaries say `BZJ4`.
- The three hand-written `vfs_journal_bytes` corpus files that start with a magic say
  `425a4a34`. The records target's corpus holds descriptions framed at check time, so it was
  already right.

## What `proof` should measure (speed-sol2-proof-fuzz)

- For each target changed here, runs per second per core at its corpus's mean length, before
  and after:
  - `api_cbor`, `api_frame`, `api_frames`, `api_http_request`, `api_http_response`;
  - `cas_remote` in mode 1;
  - the `link_*` targets;
  - `usercode_wire`, `usercode_script_output`, `usercode_script_elide`.
- The expected ratio is the table's before over after, from about 1.5× for the single-arch
  link targets to about 3.7× for `usercode_wire`.
- The mutator's cycles per run with `RecentBytes` forced, at L = 1 KiB, before and after.
  Expected: from about 35L a search to about 1 to 2 cycles a byte.
- `ascii_integer`'s and `live_patch`'s slides' shares of a run. Each closes as above if over
  10%.
- `every_case`'s wall time beside `seeds`': at most `cases` times the corpus's share of
  `seeds`.

## The model checker: crates/blaze-check/ (speed-sol2-proof-check)

The checker runs a test closure under every interleaving (bounded DPOR, loom's) or under
seeded random ones (shuttle's PCT), with C11 weak memory (Miri's store histories) and races
(FastTrack's cell shadows). All of it runs on one OS thread: model threads are coroutines, so a
thread change is a stack switch and nothing is shared across cores. Its two figures of merit
are **schedule points per second** and **executions (states) per second per core**. The
bound is the work each point and each execution needs, counted below from the code; nothing
was run (2026-10-30). Figures use "The machine" in `link/speed-of-light.md` and the top of this
doc: 3.5 GHz, about 2 instructions a cycle for branchy code (2.5 where the work is mostly
straight-line), an L1 hit in 4 to 5 cycles, L2 in about 14, a system call 0.15 µs, `mmap` plus
the first touch of a page 0.25 µs. A `malloc`/`free` pair on the allocator's fast path is
taken as about 40 cycles.

The reference workloads:
- **A litmus test** (store buffering, message passing, IRIW): 2 to 4 threads, 6 to 12 schedule
  points, 2 to 6 thread changes and 1 to 3 spawns an execution, and 10 to 1,000 executions.
- **A structure model** (a lock-free queue, a slot map, a once cell under contention): 2 to 3
  threads, 50 to 500 points with runs of 10 to 50 points on one thread, and 10^4 to 10^6
  executions under a preemption bound of 2 or 3.

The bound, in short: a schedule point is about 150 to 190 instructions on about 8 lines in
L1, about 75 cycles (21 ns), so **about 45 million points a second a core**. A litmus
execution is about 1,700 cycles (0.5 µs), so **about 2 million executions a second a core**;
a structure model's execution is its points times 21 ns plus about 0.3 µs.

### `clock.rs`: vector clocks

Work: a `Clock` is `[u32; 8]` (32 bytes, half a line). `join` is an elementwise max, `le` an
elementwise compare, `tick` one add; each schedule point does about 2 `le`s per waiting thread
(DPOR), one `join` and one `tick`, and each load does a `le` per candidate store.

- Bound: 8 lanes of u32 are one 256-bit vector. `join` is a load, a `vpmaxud` and a store,
  about 4 instructions; `le` is a `vpminud` (or `vpcmpud`), a compare and a mask test, about
  5; on aarch64 two 128-bit `umax`/`cmhs` halves, about 7.
- Today: `join` was already a zip of maxes that vectorizes. `le` was `iter().all(..)`, which
  exits early and stays scalar: 16 to 24 instructions with a branch per lane. It is now a
  branch-free `fold` with `&`, which the compiler can vectorize: about 5.
- Gap: closed (`le` was 3 to 5 times its bound, about 15% of a point with 3 threads waiting).
- `proof` should measure: the instructions of `Clock::le` and `join` at `-O` (they should be a
  handful of vector ops with no loop), and a point's cycles with 2 and 4 waiting threads.

### `path.rs`: the branch log, DPOR backtracking and PCT

Work: one `Branch` (32 bytes) pushed or replayed per schedule point, load choice and timed
wait; on a new branch the previous schedule's preemptions and active thread; DPOR's
`backtrack(point, thread)`, which marks `thread` at `point` and, with a preemption bound, at
the first schedule of the run before `point` (bounded DPOR's extra point, as loom). `step`
pops branches back to the deepest one with an alternative.

- Bound: a new schedule branch is about 50 instructions (find the previous schedule, the
  active mark among 8, preemptions, the run start, a 32-byte push); a replayed one about 25. A
  backtrack is about 15: find the schedule at or before `point`, mark one thread, load that
  schedule's run start, mark it there. `step` is about 10 instructions a popped branch,
  amortized one per branch pushed. Nothing allocates: the log is reserved once per model
  (`max_branches`, up to 64 Ki entries).
- Today, before: the extra backtrack point was found by walking `prev` links back over the
  whole run of the current thread, about 25 instructions a step with a dependent load each, so
  `25·L` for a run of `L` points. `rt::schedule` called `backtrack` again at every point for
  every thread still waiting at the same dependent access, so a run of `L` points with one
  waiting thread cost about `25·L²/2` instructions: for `L = 20`, about 250 instructions a
  point, more than the rest of the point together. A random execution made a fresh
  `Path` with its full reservation (240 KB at the default `max_branches`): an `mmap`, a first
  touch and a `munmap`, about 0.6 µs, more than a litmus execution's whole bound.
- Today, after: each `Schedule` keeps `start`, the first schedule of its run, set when the
  branch is pushed (from the previous schedule's `start` when the same thread runs on) and
  when `step` changes a branch's thread (`run_start`). The walk is one load. `rt::schedule`
  remembers per thread the access it last backtracked for (`backtracked`) and skips the repeat,
  which does the same thing (a past branch's marks don't change within an execution). Random
  mode reuses the path (`Path::random`: clear the log, reuse PCT's drop buffer), drawing the
  same seeds in the same order.
- Gap: closed. Left: `last_schedule` still finds the previous schedule by `rposition` over
  the load and wait branches since it, usually 0 to 2 entries, a few instructions.
- Tests: `path::tests::a_schedule_keeps_the_start_of_its_run` drives `Path` through 64 seeded
  sequences of branches, loads, random backtracks and `step`s and checks every `start` against
  loom's walk (`walk_start`, the reference). The existing tests (same seed, same schedules;
  replay; preemption bounds) keep the order.
- `proof` should measure: points a second on a structure model with long runs (a spin loop
  under `max_preemptions = 2`), before and after; the time of a random-mode execution of an
  empty model (should have no `mmap` in `strace`).

### `rt.rs`: a schedule point (`op` and `schedule`)

Work: every shim operation is `rt::op`: borrow the runtime (a thread-local `RefCell`), note
the operation, then `schedule`: for each thread waiting at an operation, compare the last
dependent access of its object with the thread's DPOR clock and backtrack if they are
unordered; pick the initial thread (the current one if it can run, else the least-yielded);
write the 8 marks; take or replay the branch; join and tick the chosen thread's DPOR clock and
record the access in the object's entry; reset yields.

- Bound, per point with `n` threads: TLS and borrow about 8 instructions; DPOR about 12 a
  waiting thread (entry lookup, `le`, a compare); marks about 4 a thread; the branch about 50
  (new) or 25 (replayed); the access record about 15 (a vector join, a 36-byte store, twice
  for a store); the operation itself (below) 25 to 60. With `n = 3`: about 150 to 190
  instructions on about 8 lines (the runtime's header, the thread's clocks and op, the
  object's entry, the path's tail, the newest stores), all in L1 for any model that fits in
  it: about 75 cycles, 21 ns, 45 million points a second.
- Today: at the bound after the path changes above and `le`. A warm point allocates nothing
  and makes no system call (tested).
- Gap: closed.
- Tests: `tests::an_execution_and_its_reset_allocate_nothing_when_warm` drives the runtime
  directly (no coroutines) through 250 points of every kind (store, load, RMW, SC fence,
  mutex, cell, bytes) and their reset, and measures 0 allocations for the second execution
  with `blaze-alloc-count`; `tests::a_warm_scheduling_point_allocates_nothing` does the same
  inside a model through every shim (atomics, fence, `Mutex`, `RwLock`, `Condvar`, `OnceLock`,
  `UnsafeCell`, tracked bytes, `yield_now`): 100 warm rounds, 0 allocations.
- `proof` should measure: instructions and cycles per point (`perf stat` over a litmus
  test with 10^5 executions, divided by its points), and its lines touched (`perf c2c` or L1
  misses per point, which should be near 0).

### `rt/memory.rs`: atomics (load, store, RMW, fences)

Work: each atomic keeps its stores in modification order (`Store`, 88 bytes, 1.4 lines),
up to `HISTORY` = 128 (Miri's limit). A load collects the stores coherence and the SC rules
let it read, newest first, stopping at the first that happens before it, and branches on them;
a store appends one; an RMW reads the newest; an SC store marks every store that happens
before it or that it read before as SC (Miri's rule); fences join clocks.

- Bound: a load visits each candidate store once: its stamp against the thread's clock and its
  `loaded` vector against it (one 8-lane compare), about 15 instructions and 1.4 lines a store;
  a load in a model with 1 to 4 candidates is 25 to 70 instructions. A store is a 88-byte
  append into a ring that has its capacity: about 25. An SC store adds Miri's scan, about 10
  instructions a stored entry: up to 1,280 for a full history.
- Today, before: each atomic's history was a `VecDeque` inside its object, made empty per
  execution, so the first store of every atomic in every execution allocated and later ones
  reallocated at 4, 8, 16 and so on: for a litmus execution with 2 atomics, 2 to 4
  allocations, about 160 cycles, 10% of the execution. `Store::read_before` branched per
  lane.
- Today, after: histories live in the runtime by object index (`Runtime::histories`) and are
  cleared, not freed, when the index is next made (`restart`), so a warm execution allocates
  nothing for them (the object at an index is almost always the same atomic again).
  `read_before` is branch-free. The candidate list is reserved at `HISTORY` once.
- Gap: closed for loads, stores and RMWs. The SC store's scan is the work of Miri's rule
  itself, so it is at its bound; that rule costs O(history) per SC store, which matters only
  for SC-heavy spin loops near 128 stores. A rule that keeps the position up to which every
  store is already SC would make it O(new stores), but it is a change to the memory model's
  port and needs a differential test against Miri first; it is left, and `proof` should say
  whether any real model reaches it.
- `proof` should measure: cycles of a load against its candidate count (should be linear at
  about 6 cycles a candidate), and the time of an SC store against history length.

### `rt/memory.rs`: cells and tracked bytes (races)

Work: each `UnsafeCell` keeps its last write and each thread's last read (FastTrack's shadow:
a `Stamp` and a `Clock`). A read checks the last write against the thread's clock; a write
checks the reads and the write. `cell::read`, `write` and `free` track raw bytes exactly to the
byte: one shadow per run of bytes accessed alike.

- Bound: a cell access is a `Stamp` compare and, for a write, a `le` on the reads: about 15
  instructions and 1 line. A byte access over `k` runs is a binary search (about 5 probes in a
  model's tens of runs), at most two splits, the `k` checks and one replace or fill: about
  40 + 20·k instructions, no allocation once the run list has its capacity.
- Today, before: the runs were a `BTreeMap` from start to run, rebuilt every execution: each
  node an allocation (about 40 cycles each, about 4 to 8 a model's execution) and each
  lookup a tree walk with a dependent load per level. A cell write checked the reads lane by
  lane.
- Today, after: `ByteMap` is a sorted `Vec<Span>` cleared per execution (capacity kept), with a
  `partition_point` lookup, `split`, `replace` (one insert or an overwrite and drain) and `fill`
  (inserts for the gaps a read covers, using a reused scratch list). A cell write compares the
  reads with one `le`, and only calls the race report when it fails.
- Gap: closed.
- Tests: `rt::memory::tests::the_byte_map_keeps_what_a_shadow_per_byte_keeps` runs 32 seeds of
  200 random writes and reads of a 64-byte buffer against a shadow per byte (the reference) and
  checks the runs are sorted, disjoint and keep each byte's write and read; the existing race,
  use-after-free and double-free tests keep the findings.
- `proof` should measure: a model that tracks a buffer with 10 to 100 runs (should be flat in
  allocations and near 60 instructions an access).

### `rt/locks.rs` and `sync/lock.rs`: mutexes, read-write locks, condition variables, once

Work: a lock is a schedule point, then the model's holder check and clock join; an unlock
copies the holder's clock into the lock and wakes waiters. A condvar wait releases, blocks and
reacquires; a notify wakes one or all waiters in order. The shim also takes std's own lock of
the same kind, uncontended, so a value reached from an OS thread outside the model (a static
the test also uses outside `model`) stays sound.

- Bound: the model's lock and unlock are about 40 instructions each plus two schedule points;
  std's uncontended lock is a `cmpxchg` (about 18 cycles on Zen) and its unlock an `xchg`
  (about 18): about 36 cycles a pair, about 17 to 20% of a pair's model cost (about 200
  cycles). A notify of `w` waiters is about 10 + 5·w instructions.
- Today, before: each condvar's waiters were a `Vec` made per execution: an allocation on the
  first wait of each execution, and a `remove(0)` per notify.
- Today, after: `Condvar` keeps `[u8; MAX_THREADS]` and a length: a notify copies the woken
  ones out and shifts the rest down with `copy_within`, same order (FIFO), no allocation.
- Gap: std's lock stays, at about 17 to 20% of a lock pair. It is the soundness backstop for
  a value shared with a non-model OS thread, and loom does the same (its `Mutex` wraps std's).
  Dropping it would need a proof that no shim object is ever reached outside its model, which
  the public API does not give. Everything else is at its bound.
- `proof` should measure: cycles per lock/unlock pair inside a model, and the share of them in
  `std::sync::Mutex::lock`/`unlock` (expected about 36 cycles).

### `coro.rs`: coroutines and thread changes

Work: a thread change saves the callee-saved registers on the stack it leaves and loads them
from the one it enters. On x86-64 that is 6 general registers plus MXCSR and the x87 control
word (`stmxcsr`/`fnstcw` and their loads, which the SysV ABI makes callee-saved); on aarch64
12 general and 8 vector registers. On Windows it is `SwitchToFiber`.

- Bound: one switch is about 20 instructions; on x86 the MXCSR and x87 control word loads
  are microcoded, about 20 of a switch's about 35 cycles; aarch64 about 25 cycles. A thread
  change needs exactly one switch.
- Today, before: every thread change went through the explorer: the running coroutine
  suspended to the explorer (a switch, and a return-stack misprediction), the explorer
  borrowed the runtime, checked for a finding and new threads, and resumed the next one (a
  second switch): about 110 cycles. That is 13% of a litmus execution (4 changes) and about
  2.5% of a structure model's.
- Today, after: `rt::go_on` switches straight to the next thread when no thread was spawned
  since the explorer last looked (`coro::switch_to`): the explorer's saved context goes with
  the switch (`hand_over` copies `back`), so the target's next `suspend` returns to the
  explorer as if the explorer had resumed it. `resume` reports which coroutine came back.
  Coroutines are registered by index (`ALL`), so finding the target is one load. About 50
  cycles a change. `Job::Main` is one `Rc` per model, so starting thread 0 allocates nothing.
  `Shared` is `Box::into_raw`, fixed for the coroutine's life; a coroutine left busy at a
  finding is leaked, stack and `Shared`, as before.
- Gap: closed. Left: x86's MXCSR and x87 saves, which the ABI requires (boost.context and
  loom's `generator` pay them too).
- Tests: `tests::threads_switching_to_each_other_explore_what_the_explorer_does` runs a
  3-thread model with direct switches and again with every change through the explorer
  (`coro::DIRECT`, a test-only switch, the reference) and checks the two give the same
  executions in the same order, step for step.
- `proof` should measure: cycles per thread change (a ping-pong model of two threads over one
  atomic), and the number of explorer loop turns per execution (should be about one per spawn
  and one per thread end, none per thread change).

### `explore.rs`: an execution and the reset between executions

Work: per execution, `Runtime::begin` (clear threads and objects, push thread 0 and the 9
reserved objects, clear the byte runs), start thread 0's job, run, check the path was replayed
in full, `end`, `Path::step`.

- Bound: `begin` writes about 9 entries of about 100 bytes and one `Thread` of about 300 bytes:
  about 200 instructions, 80 cycles. `step` is about 10 instructions a popped branch. No
  allocation and no system call. For a litmus execution, with 10 points (750 cycles), 4
  changes (200), 2 spawns (480, below), and the reset (about 250), about 1,700 cycles: 0.5 µs,
  2 million executions a second a core.
- Today, before: per execution, a `Box` for the main closure, the atomics' and condvars'
  first allocations (above), the byte map's nodes, and, in random mode, a whole new path: in
  all, 3 to 10 allocations (120 to 400 cycles, 7 to 25%) and, in random mode, an `mmap` and
  `munmap` (about 0.6 µs, more than 100%).
- Today, after: the main job is built once per model, objects are cleared and their heap
  parts kept by index in the runtime, the path is reused in both modes. Executions after the
  first allocate only what the program does: a spawn's body and result (as std's `spawn`) and
  a thread's first `thread_local!` value and its list.
- Gap: closed.
- Tests: `tests::executions_after_the_first_allocate_nothing` runs the same random-mode model
  for 2 and for 50 executions and checks both allocate the same; the runtime-level reset test
  above measures 0 for a whole execution and reset.
- `proof` should measure: executions a second for an empty model (the reset alone; expected
  about 5 to 10 million), and for the store-buffering litmus test (expected about 2 million).

### `thread.rs`: spawn, join and thread-locals

Work: a spawn boxes the closure and makes the result slot, adds a thread to the runtime (its
clock joined from the parent's), and readies a coroutine (made once per model and index, then
reused); a join is a schedule point and a clock join.

- Bound: 2 allocations (the body `Box` and the result `Arc`, as `std::thread::spawn`, about 80
  cycles) plus about 100 instructions in the runtime: about 240 cycles a spawn. A join is a
  schedule point plus a `join`. A thread's first `thread_local!` value is one `Rc` and, the
  first time, its list's allocation.
- Today: at the bound; coroutines and their stacks are reused (one `mmap` per model thread
  per model, not per execution).
- Gap: none over 10%. The 2 allocations a spawn could go into a per-model arena, but they are
  the program's work under std too (the same closure and packet), so they are not overhead
  the checker adds.
- `proof` should measure: allocations per spawn (2) and the spawn's share of a litmus
  execution.

### `sync/atomic.rs` and `sync/once.rs`: the shims

Work: each shim atomic holds the real `std` atomic (the value a non-model thread or a final
`get_mut` sees), its id (an `AtomicU64`: the execution's epoch and the object's index) and its
initial value. An operation finds its object (compare the id's epoch, else make it), then is
`rt::op` with the real operation done after the point. `OnceLock` and `Once` are a lock-like
object plus std's own cell.

- Bound: finding the object is one load and compare, about 4 instructions; the rest is the
  schedule point. Outside a model (a shim used on a plain OS thread in a model build), one TLS
  load and a branch, then the std operation.
- Today: at the bound.
- Gap: none. The shim types are bigger than std's (an `AtomicU32` is 16 bytes, not 4), which
  only model builds see.
- `proof` should measure: nothing beyond the schedule point; the outside-a-model path should be
  within a few cycles of std's.

### `cell.rs` and `hint.rs`

Work: `UnsafeCell::with`/`with_mut` find the cell's object and do the race check (no schedule
point: a plain access is not a branch, as in loom); `cell::read`/`write`/`free` are the byte
tracker above; `hint::spin_loop` yields.

- Bound: about 20 instructions a cell access; the byte tracker above.
- Today: at the bound.
- Gap: none.

### `linear.rs`: linearizability

Work: given a history of `n` operations (each a call and a return, ordered by happens-before),
search for an order of them that respects happens-before and that the sequential
specification accepts (Wing and Gong's search with Lowe's memo of (done set, state) pairs).

- Bound: each (done set, state) pair is visited once; a visit is a hash of the set and the
  state, a probe, and one clone of the state per child tried. The search is exponential in the
  worst case (NP-complete in general), so the bound is per visited pair: one hash and probe
  (about 30 cycles for a small state) and one clone per child.
- Today, before: the memo was a `HashSet<(u64, S)>` that kept a clone of each state it saw,
  probed with a fresh clone, and the search cloned the state again for each child.
- Today, after: `Memo` keeps each seen state once in a `Vec` and an `FxHashMap` from the
  hash to the first entry of its chain; `insert` hashes the pair with `FxBuildHasher`, walks
  the chain, and only on a miss stores it. The search clones the parent's state once per child
  and recurses only on a new pair.
- Gap: closed for states that own heap memory (a `Vec` model: a clone is an allocation, about
  40 cycles plus the copy, against a hash of about 10). For a state that is a small `Copy`
  value, the extra hash is a few cycles over the old cost, under 10% of a visit.
- `proof` should measure: visits a second and allocations per visit on the queue and register
  specs in `tests.rs`.

### Without `cfg(blaze_check)`: the switch

Every crate that model-tests its code has a `sync.rs` that re-exports `std`'s types under
`cfg(not(blaze_check))` and `blaze_check`'s under `cfg(blaze_check)`, with `#[inline(always)]`
no-op twins of `cell::read`, `write` and `free`. `blaze-check` is a target-`cfg` dependency, so
a normal build does not even build it.

- Re-exports: the same types as `std` (`pub use std::sync::...`): 0 instructions, 0 bytes,
  the same layout and codegen.
- The no-op twins: 0 instructions at `-O` (inlined, arguments without side effects removed);
  at `-O0` the always-inliner leaves the argument evaluation, a few `mov`s per call.
- The dependency: none; `blaze-check`'s code is absent from the binary.
- In a model build, outside `model`: a shim costs one TLS load and a branch over std's, and its
  types are bigger (above).
- Gap: none. `proof` should check the `-O` codegen of a crate's hot function using `sync.rs` is
  byte-identical with and without the re-export (it is the same type, so it should be).

### The state of the art

- **loom** (tokio): the design followed here (bounded DPOR with loom's backtracking, `Path` of
  branches, objects by index, coroutines on one thread). Its per-execution state is plain
  collections and fixed arrays sized by `MAX_THREADS`, with no bump arena. It runs every
  thread change through its scheduler (two `generator` switches) and walks back for the
  bounded-DPOR point. Its C11 model is coarser: 7 stores kept per atomic, and `SeqCst`
  treated as `AcqRel` (its documented limit). This checker is at or past it on each: one
  switch a change, an O(1) backtrack point, no per-execution allocation, and Miri's weak
  memory.
- **shuttle** (AWS): random and PCT schedulers, no DPOR, and sequentially consistent memory
  only; very cheap points (no clocks), so it can be faster per point on SC-only models, but it
  checks less. Its PCT is what `Mode::Random` follows, with blaze's SplitMix64.
- **GenMC** (MPI-SWS): stateless model checking over execution graphs for RC11 and IMM, optimal
  (each consistent execution once) where DPOR explores some twice; it runs on LLVM IR, so
  it is not a library a Rust test can call. Its per-execution cost is graph-consistency checks
  rather than schedule points; for a weak-memory model it explores fewer executions than this
  checker. Matching it would mean a different search, not a faster point.
- **CDSChecker** (UC Irvine): C11 model checking with DPOR over modification-order
  constraints and promises, per-execution state in its own snapshotting allocator (forks or
  `mprotect` snapshots); this checker's reset is cheaper (a clear of reused tables, no
  snapshot), and like loom it re-executes from the start rather than snapshotting.
- **Miri**: the weak-memory and race rules ported here; it runs one random interleaving per
  run, with no search, at an interpreter's cost (10^2 to 10^3 times native per operation).

### Memory by lifetime

- Per model: the runtime (`Box<Runtime>`), the path reservation (32 bytes times
  `max_branches`, up to 64 Ki), one coroutine (`Shared` and a stack `mmap` with a guard page)
  per model thread index, the main job's `Rc`, PCT's drop buffer.
- Kept across executions, by index: threads, objects, store histories (up to 128 stores of 88
  bytes per atomic), candidates (128), byte runs and scratch, condvars' fixed waiters arrays.
- Per execution: only the program's own allocations: a spawn's body and result, thread-local
  values.
- Per point: nothing.

### What `proof` should measure

- Points a second a core and executions a second a core on the store-buffering litmus test,
  a structure model with long runs under `max_preemptions = 2`, and an empty model: targets 45
  million points, 2 million litmus executions, 5 to 10 million empty executions.
- Cycles per thread change (ping-pong of two threads): target about 50 on x86, 40 on aarch64.
- Allocations: 0 per warm point and per reset (the tests above, under `blaze-alloc-count`),
  2 per spawn; no `mmap` per execution in random mode.
- The share of a lock pair in std's lock (expected about 36 cycles, the one gap left over
  10%), and an SC store's cost against history length.
- Determinism: the same seed gives the same schedules in the same order before and after (the
  existing replay and seed tests, plus the direct-switch differential).
