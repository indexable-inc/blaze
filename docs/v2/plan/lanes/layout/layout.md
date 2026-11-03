---
status: plan
summary: Profile-guided code layout in blaze's own linker: C3 function order from an ix-prof sampling profile mapped back through per-build address maps, cold functions inferred without a profile and placed last, cold blocks weighted unlikely; hot/cold splitting designed, not built; the benchmark.
date: 2026-11-02
---
# Profile-guided code layout

Lane `layout`, 2026-11-02. blaze lays out the code of every image it links by a profile of
that image's runs whenever there is one, and by what codegen infers cold when there is none.
Three parts were asked for; two are built, one is designed here and not built:

| Part | State |
|---|---|
| Function order: hot functions packed in call order (C3), cold ones last | built (`crates/blaze-link/src/layout.rs`) |
| Cold inference: panics, `?`'s `Err` path, `#[track_caller]` panics, alloc errors, unwinding | built: cold functions last, cold blocks weighted unlikely in LLVM |
| Hot/cold splitting: each function's cold blocks in a separate cold section | designed below, not built |
| Profiles of programs blaze builds | built: ix-prof samples, mapped back through address maps blaze-link writes |

## Using it

```sh
mkdir -p -m 700 target/layout          # opt in: links write address maps there; IX_PROF needs 0700
blaze build --release                  # links once more (the key changed), writes the map
IX_PROF=target/layout ./target/release/app <workload>   # writes app-<pid>.ixprof there
blaze build --release                  # relinks laid out by the profile
```

A program other than blaze links ix-prof and calls `ix_prof::start_from_env()` in `main`;
blaze does that itself (`crates/blaze-engine/src/program.rs`), for the client and for the
daemon it starts, which inherits `IX_PROF`. A daemon writes its profile when it ends, so to
profile blaze building something run the build with `IX_PROF` set, then `blaze stop` with the
same `BLAZE_HOME`.

With no `target/layout` nothing is written or read and a link costs what it did. Without a
profile the cold functions still go last: that needs no directory.

## Function order

A full link sorts each code class's functions by a three-word key; the middle word
(`crates/blaze-link/src/place.rs`, `order_functions`) is the tier:

1. Functions the profile saw run, in C3's order: the place, below 2^32.
2. Every other function: `NEUTRAL` (1 << 32), by content as before.
3. Functions codegen marked cold that the profile did not see run: `COLD` (2 << 32), by content.

The profile wins: a function it saw run is placed hot whatever codegen inferred (the span's
`overruled` counts them).

C3 is Ottoni and Maher, "Optimizing Function Placement for Large-Scale Data-Center
Applications" (CGO 2017), as lld's `--call-graph-profile-sort` does it
(`lld/ELF/CallGraphSort.cpp`), which is also what BOLT's `-reorder-functions=cdsort`
descends from and what Propeller uses for function order:

- A node's weight is its own samples (it was the leaf); an edge's is the samples whose leaf
  is the callee and whose next physical frame is the caller. lld weighs nodes by incoming
  call counts from an instrumented profile; samples are the sampling analogue.
- Nodes go densest first (weight over size, compared as exact `u128` cross-products). Each
  joins its heaviest caller's cluster unless that call is no more than a tenth of its own
  weight, the merged cluster would pass 1 MiB (`MAX_CLUSTER_SIZE`), or the merge would leave
  the cluster under an eighth of the caller cluster's density (`MAX_DENSITY_DEGRADATION`).
- Clusters go densest first, each in its merge order.
- Every tie falls to instance key, so the order is a function of the keys, sizes, heat and
  cold set alone.

The pass runs once per stage, under `link.layout`, which records the profile's id and the
counts (`functions`, `hot`, `clusters`, `cold`, `overruled`, `unmatched`) or `skipped` with
why: no profile and nothing inferred cold; an incremental link (new code goes to the patch
region, so the order waits for the next full link); a wasm image (code is reached by index).

## Cold inference

Without a profile, codegen marks what is cold and the profile, when there is one, overrides it.

Cold functions (`cold_of` in `crates/hir-ty-codegen/v2/frontend.rs`, `cold_fn` in
`crates/hir-ty-codegen/mod.rs`), carried as `Described::cold` and `Built::cold` into
`Change::cold`:

- `#[cold]` functions, which covers `core::panicking`'s and `alloc::alloc::handle_alloc_error`'s
  paths in the real std;
- the `panic_impl`, `begin_panic` and `eh_personality` lang items;
- `#[track_caller]` functions that return `!` (`panic!`'s and `unwrap`'s failure paths).

Cold blocks (the block marking in `crates/hir-ty-codegen/mod.rs`, `calls_cold`):

- MIR cleanup blocks (unwinding);
- blocks that call a cold function, or `FromResidual::from_residual` (the lang item
  `TryTraitFromResidual`) into a `Result`, which is `?`'s `Err` return. `?` on an `Option` or
  a `ControlFlow` is not cold: a `None` or a `Break` is as often the answer as the error;
- the front end already marked panic blocks and landing pads cold.

A cold block becomes branch weights in LLVM (`crates/hir-ty-codegen/llvm/lower.rs`,
`unlikely`): 1 against 2000, LLVM's `UnlikelyBranchWeight` and `LikelyBranchWeight`, so LLVM
moves the block out of the fall-through path and to the function's end. `fold.rs` hashes
`is_cold` per block, so the marks are in the code's identity. A mark reads the callee's
attributes and signature, which a caller's key already covers: an item's form hashes every
attribute flag but documentation's and the IDE's (`crates/blaze-items/src/form.rs`,
`unobserved`), `#[cold]` and `#[track_caller]` among them. The stencil tier ignores the
marks: its block order is the MIR's.

## Profiles: ix-prof samples through address maps

ix-prof records CPU samples by stack; each native frame carries the build id of the image it
is in (Mach-O `LC_UUID`, ELF `NT_GNU_BUILD_ID`) and its address in the file (the slide taken
off; a return address less one). blaze-link writes both build ids from the image's identity:
`H("blaze v2 link image 1", lineage, generation)[..16]` (`Image::build_id`).

While `<target>/layout` exists, each link of an executable or a non-proc-macro shared
library writes its address map (`crates/blaze-cargo/src/serve/unit/layout.rs`):

- at `<target>/layout/maps/<file name>-<16 hex of H("blaze v2 layout image 1", path)>/<32 hex build id>`;
- as `blzlmap1`, the image path's length (u32 LE) and bytes, a row count (u64 LE), then rows
  of address (u64), size (u64) and function identity (32 bytes), by address;
- whole or not at all: written to `.part-<pid>`, then renamed; a map already there under
  the build id with other rows is refused, never written over (see the build id under "What
  is missing");
- collected after each write: maps a profile names are kept, and of the rest the newest
  four.

A map names each function by its identity (`Described::identity`: item, instance, type
arguments; a folded group's body by its members' identities, sorted, from
`crates/blaze-codegen/src/driver.rs`), not by instance key or symbol name; a function of no
code has no row: an edit of the body changes the key but not
the identity, and so does a toolchain bump, so one profile keeps laying out the following
builds as their code changes. Names are not unique; identities are.

Reading (`Profiles::read`, once per job, under `layout.profiles`, which records files,
samples, unmapped, outside and images): every `*.ixprof` in the directory, sorted; on-CPU
samples only; the leaf is the first physical frame (frames with one build and address are
one machine frame, the functions inlined there; span frames skipped) and the caller the next
one when in the same build. Only that pair is an edge: a function on the stack but not
running executes little, and its own samples, not the time under it, are what fills the
I-cache; lld's call counts weigh a node by its calls in, which a sampling profile does not
have. A sample whose build no map has is counted `unmapped`, one in a
mapped build but no function `outside`; neither is guessed at. The result is each image's
`Weights` by identity, which a link turns into a `Heat` by instance key through the build's
`Built::identities`; an identity the build has no instance of is dropped. A profile that does
not parse stops the build naming the file ("remove it, or profile again"); ix-prof writes
each profile to `<name>.part` and renames it, so a build reading the directory while a
profiled program ends never meets half a file.

## Keys and determinism

- The unit's record key (`RecordInputs::layout` in `crates/blaze-cargo/src/serve/unit/compile.rs`)
  takes `H("blaze v2 layout key 1", for each linked path with weights: path, weights id)`.
  It is there whenever `layout` exists, so opting in relinks once and the map gets written,
  and a new profile relinks.
- The kept image (`codegen.rs`, `kept_image`) remembers the weights id its full link used. A
  link whose weights id differs takes a fresh image, reported as `Full::Layout` ("not spliced:
  the profile its code is laid out by changed"). It compares the weights id, not the heat, so
  an edit, which changes instance keys and so the heat, still splices.
- The image's lineage (domain "blaze v2 link lineage 3") takes `Pass::id`: the heat's id and
  the sorted cold set. So the build id names the order.
- Weights and heat are sorted and summed; every tie in C3 is by instance key; the maps' rows
  are by address and key. The same tree and profiles give the same bytes
  (`the_same_tree_and_profile_give_the_same_bytes`).

## The guarantee relied on

Rust leaves function addresses and their order unspecified. The Reference's "Function pointer
types" gives a function pointer no address guarantee, and `core::ptr::fn_addr_eq` documents
that one function may have several addresses and two functions may share one. No program the
pinned nightly builds can depend on function order, so reordering changes no behaviour.
Branch weights change no semantics: they are hints to LLVM's block placement.

## Hot/cold splitting: designed, not built

Each function's cold blocks would go to a part of their own in a cold region after all hot
code, the way LLVM's machine function splitter (`-split-machine-functions`, sections
`.text.split.<name>`), LLVM's IR `HotColdSplitting` (outlined `<name>.cold.N` functions),
BOLT's `-split-functions` and GCC's `.text.unlikely` / `<name>.cold` do it. What it takes in
blaze, and why it is not in this change:

1. Codegen: LLVM's splitter needs block counts. With `!prof` weights only it splits nothing,
   so blaze would ask it to split by its cold marks (as `-mfs-split-ehcode` does for EH pads),
   or outline at the MIR level. The stencil tier would need its own splitting.
2. The linker: the cold part is a piece with its own symbol, placed in the `COLD` tier of the
   code class, with relocations between the parts (branches both ways).
3. Unwinding, per format:
   - DWARF CFI: a second FDE for the cold part, its CFA rules from the split point. Every
     landing pad of a function in one part, and that part's start as the LSDA's `LPStart`
     (encoded, not omitted), with each part's call-site table naming only its own calls.
   - Mach-O: an `__unwind_info` entry for the cold part, which compact unwind can rarely
     express, so a DWARF fallback entry (`UNWIND_*_MODE_DWARF`).
   - PE: a `RUNTIME_FUNCTION` for the cold part with chained unwind info
     (`UNW_FLAG_CHAININFO`) to the hot part's.
4. Debug info: `DW_AT_ranges` instead of `low_pc`/`high_pc` for a split function, and line
   tables for both parts.
5. Live patching: a patch replaces both parts or neither.

Each of these is a way to break unwinding through a panic, which the pinned nightly's
behaviour depends on, and the three formats' writers are other lanes' active code. Function
order and LLVM's own block placement deliver most of the I-cache and iTLB win at none of that
risk, so splitting waits for a lane of its own.

## What is missing

- **`--prof` for any program.** blaze cannot yet profile a program it builds without that
  program linking ix-prof itself; the pprof lane's `blaze run --prof` is designed, not built.
- **Shared libraries.** ix-prof gives only the main executable a build id, so a cdylib's
  samples are `unmapped`; maps are written for them already.
- **The daemon's own samples** (`blaze-profile`'s table) are by name, not address, so they
  cannot feed the order; ix-prof's can.
- **`.profdata`.** LLVM instrumented profiles (`crates/blaze-lints/src/profile/indexed.rs`
  reads them) do not feed function order; they could become `Weights` directly.
- **The stencil tier** ignores cold blocks.
- **A record hit writes no map.** A unit whose record hits restores its binary and links
  nothing, so it writes no map; when collection had removed that build's map (an old tree
  checked out again), a run of it is `unmapped`. Fix: the map as one of the record's
  outputs.
- **The build id** is `H(lineage, generation)`: two daemons can give one id to two
  different incremental links of one lineage (a restart, the tree put back, another edit).
  The map writer refuses the second map, naming it; the fix is the id, which should chain
  each incremental link's change into it.
- **Map collection** keeps the newest four unnamed maps by mtime; a `.part-<pid>` a killed
  link left stays. Two builds of one image
  collecting at once can remove a map the other's profile is about to need; that run's
  samples are then `unmapped`, not misread.
- **Reading cost.** Profiles are read on the job's thread, once per job that links, with no
  stat cache, so a large profile directory costs every such job.
- **Hot/cold splitting** (above).

## Tests

- `crates/blaze-link/src/layout.rs`: `a_hot_callee_follows_its_caller`,
  `cold_goes_last_unless_the_profile_saw_it_run`, `inputs_in_any_order_give_one_order`,
  `clusters_stop_at_a_mebibyte`.
- `crates/blaze-link/src/tests/layout.rs` (a full ELF link):
  `a_profile_packs_hot_functions_first_and_cold_last`,
  `the_same_tree_and_profile_give_the_same_bytes`,
  `an_incremental_link_moves_nothing_for_a_profile`.
- `crates/blaze-cargo/src/serve/unit/layout/tests.rs`:
  `a_sampled_address_names_the_function_its_build_put_there` (map written, profile read
  through it, inlined frames one frame, other builds and outside addresses counted, the next
  build's keys taking the weight by identity).
- `crates/blaze-cargo/src/serve/unit/codegen/tests.rs`:
  `a_kept_image_is_taken_only_for_its_own_output_and_layout`.

## The benchmark

`tools/layout/bench.py` (typed input, `tools/layout/bench.json`): blaze building a fixed
workload (ripgrep at af60c2de9d85e7f3d81c78601669468cf02dabab, `blaze build`), the measured
program being blaze itself, built from one tree three ways:

- `before`: built by a blaze of the merge base (no layout, no cold inference);
- `cold`: built by a blaze of this change with an empty `layout` (cold inference only);
- `profiled`: the same after one training run of `cold` on the workload under `IX_PROF`.

Each binary runs the workload with a fresh `BLAZE_HOME` and target directory, rotated each
round, after one warm-up each; the script records the tree's digest before and after (they
must match), every builder's and binary's sha256, each round's time, the medians and minimums,
and the speedups over `before`. It refuses a `profiled` binary equal to `cold`'s but for the
build id. `cold` against `profiled` isolates the profile; `before` against `cold` is every
difference between the two builders, so its builder is to be this tree with the layout
change reverted, not an older main. Results go here, quoted from the spans and the script's
output, when the coordinator has run it.
