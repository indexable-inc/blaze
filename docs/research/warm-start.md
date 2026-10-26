# A daemon's start on a warm store, with nothing of rustc's

2026-10-24, for [#41](https://github.com/indexable-inc/ra-jit/issues/41) (the workspace's own
state on disk). Measured on dev-aws-7 (c7i.16xlarge, 64 cores, 124 GiB, Linux x86-64),
Cranelift, `RA_JIT_RUSTC_BASE=0`, with `harness/warmstart.py` on bench.py's helix, ripgrep and
nushell. Before is main at `b6e58ceab9`; after is the warm-manifest branch on it. Each claim is
**read** (measured, or read in a source), **reported** (someone says so) or **unverified** (our
inference).

## Short version

- #41 asked for the workspace's own state on disk because a restart was thought to spend its
  time inferring again. It does not. In helix's "again" (the store has every function, the
  daemon starts anew), inference was 5.2 s of thread time and the first build 51.2 s of wall
  time (**read**).
- The store saved nothing on helix. "restart" found nothing in the store and compiled 148,105
  functions in 69.6 s. "again" took 118,698 entries from the store, compiled 50,440 instances
  and took 105.1 s (**read**).
- Three costs made it so, none of them reading from disk (**read**):
  1. The walk of the types an instance mentions, which picks the crate it is compiled as
     (`home::Mentions`), went into a type as often as the type was reached. A future's type
     holds the futures it awaits, so the same few types were reached along exponentially many
     ways. One thread held the build for about 25 s while 63 were idle, and the main thread's
     `resolve` phase took 11.4 s.
  2. The 64 compiling threads read the same store entries and sets of types at the same time,
     and interning a value takes a shard's write lock even when the value is already in it:
     571 s of thread time in "store read".
  3. The first edit after a start rebuilt the checkout's code from the store, so that the edit
     after it has marks to patch against: 46.2 s, waited for before the edit was answered.
- After, helix's "again" takes 38.0 s, its first edit's rebuild 14.0 s, "restart" 34.3 s and a
  second checkout's wait until ready 59.8 s (was 105.1 s). nushell's "again" is 35.8 s (was
  37.8 s) and ripgrep's 6.2 s (was 6.3 s). Every program's output is cargo's (**read**).

## How it was measured

- `harness/warmstart.py PROJECT` gives every build its own home and one store, with
  `RA_JIT_CACHE`, `RA_JIT_RUSTC_BASE=0` and `RA_JIT_STATS=1`. Its steps are `first` (an empty
  store), `warm` (a second checkout of the same commit on that store), `restart` (the second
  checkout's daemon stopped and started) and `again` (once more). After each it makes a body
  edit and undoes it, and checks the program's output against cargo's (**read**).
- The time of a step is `cargo ra-jit build` plus the wait until `cargo ra-jit status` says
  the checkout is no longer loading, so a rebuild for the first edit is in it. What each worker
  said is in `WORK/said-STEP.txt`, and for the edits in `said-STEP-edit.txt` (**read**).
- The stacks are from `gdb -p` on the worker every 3 s (`thread apply all bt 45`). Frames are
  cut at 45, so a deep walk shows no caller. perf with `-g` names nothing: the release build
  has no frame pointers (**read**).

## Before and after

Seconds, Cranelift, one run each (**read**). "ready" is the wait after `cargo ra-jit build`;
"build" includes it for the daemon's own builds (`restart`, `again`).

| | helix before | helix after | nushell before | nushell after | ripgrep before | ripgrep after |
|---|---|---|---|---|---|---|
| first: build + ready | 48.5 + 62.2 | 47.5 + 26.2 | 55.7 + 24.9 | 56.2 + 24.6 | 15.5 + 2.8 | 14.5 + 2.8 |
| warm: build + ready | 39.3 + 105.1 | 38.0 + 59.8 | 43.6 + 35.2 | 43.5 + 31.6 | 6.7 + 3.6 | 6.6 + 3.3 |
| restart | 69.6 | 34.3 | 34.0 | 33.6 | 5.8 | 5.7 |
| again | 105.1 | 38.0 | 37.8 | 35.8 | 6.3 | 6.2 |
| edit after first | 0.18 | 0.16 | 2.12 | 0.37 | 0.09 | 0.09 |

helix's "again", from what its worker said (**read**):

| | before | after |
|---|---|---|
| first build's compile | 51.2 s | 16.4 s |
| main thread's `resolve` | 11.4 s | 0.66 s |
| "store read", thread time | 571.2 s | 129.4 s |
| the rebuild for the first edit | 46.2 s | 14.0 s |
| its `resolve` | 11.8 s | 1.0 s |

## Again, on main 81 commits later

The branch merged with main at `c48ca3f800` (2026-10-24), measured at `954c75add0` against
`c48ca3f800` itself, on the same dev-aws-7 (c7i.16xlarge, Xeon Platinum 8488C, 64 cores, Linux
x86-64), Cranelift, release builds, one run each of `harness/warmstart.py helix` and
`harness/warmstart.py nushell` (**read**). Every program's output was cargo's, and on the
branch's build `frozenwarm.py` passed and `stress.py` had 100 of 100 edits right (**read**).

| | helix before | helix after | nushell before | nushell after |
|---|---|---|---|---|
| first: build + ready | 48.4 + 60.7 | 49.5 + 25.7 | 55.8 + 25.2 | 56.4 + 25.0 |
| warm: build + ready | 40.5 + 113.0 | 40.8 + 59.0 | 43.6 + 34.3 | 42.7 + 31.6 |
| restart | 69.6 | 34.5 | 34.1 | 33.8 |
| again | 106.5 | 38.3 | 37.8 | 35.4 |
| edit after first | 3.57 | 0.18 | 1.07 | 0.37 |

helix's "again", from what its worker said (**read**):

| | before | after |
|---|---|---|
| first build's compile | 51.9 s | 17.0 s |
| main thread's `resolve` | 11.4 s | 0.71 s |
| "store read", thread time | 561.1 s | 130.8 s |
| the rebuild for the first edit | 45.5 s | 12.3 s |
| its `resolve` | 11.8 s | 1.1 s |

nushell's "again" has little of what this change removes: "store read" 61.0 s of thread time
before and 45.3 s after, its first build's compile 16.2 s and 14.4 s (**read**).

So helix's gains held through main's 81 commits, among them compile-ahead giving up once symbols
are read and the standard library fetched into an empty store (**read**). The counts in "not
kept" and "not found" are the same before and after (1,615 and 24,769), as expected: this
change reads the store faster, it does not keep more in it (**read**).

## What changed

- `home::Mentions` and `returns_async_block`'s walk go into each type once. The other walks of
  types in `hir-ty-codegen` already did (**read**).
- `Entries::get` and `Entries::types` read each entry and set of types once. A thread that asks
  while another reads it waits for that read (**read**).
- The interner (`intern::Interned::new`, `new_gc`, `InternedSlice::from_header_and_slice`,
  `Symbol::intern`) looks a value up under the shard's read lock and takes the write lock only
  to insert one. A value removed by a drop or a collection is removed under the write lock as
  before (**read**).
- The entries read from the store are freed on another thread, as the types told of were. That
  was 6 s or more of the main thread after helix's build (**read**, three samples).
- An edit does not wait for the store to have what the build before it kept, and each write
  waits for the one before. After a build from nothing, helix's write took 21.2 s, longer than
  the build's compile (20.4 s), and the edit after it waited for all of it: 21.3 s. That was the
  first measurement's 16.8 s edit after `first` (**read**).
- A worker whose input ends (the daemon stopping) waits for its store writes before it exits,
  and the daemon gives each a minute. Without that, a stop just after an edit lost what the
  build before it kept: helix's `warm` compiled 131,609 functions instead of 115,706 (**read**).

## Why "restart" reused nothing (2026-10-25)

Measured on dev-aws-5 (c8i.16xlarge, 64 vCPUs, Linux x86-64), each run pinned to 16 cores with
`taskset -c 32-47`, Cranelift, one run each of `harness/warmstart.py`. Before is main at
`d4f6027e82`, after is the warm-store-1024 branch on it (**read**).

- The store hashes the toolchain's name into every key. A build told ahead of cargo's (`first`,
  `warm`) named it by the sysroot's libstd rlib; a daemon's own start (`restart`, `again`) named
  it by rustc's link line, which has libcore and liballoc as well. So `restart`, the first
  daemon-own start after cold builds, read a keyspace nobody had written: 0 functions, 0 frozen
  def maps, 0 proc-macro expansions. `again` found what `restart` had written (**read**: the
  two strings in `toolchain_identity`, and what the workers said).
- Both now name core, alloc and std by their metadata hashes (**read**).

| nushell `restart` | before | after |
|---|---|---|
| functions found / compiled and added | 0 / 157,057 | 103,750 / 0 |
| frozen crates from the store / written | 0 / 1,297 parts | 374 / 0 |
| proc macros expanded / from the store | 1,605 / 0 | 16 / 1,586 |
| cold compile | 17.9 s | 11.9 s |
| `cargo ra-jit build`, ready included | 31.3 s | 30.7 s |

ripgrep's `restart` went from 0 found and 16,826 added to 9,640 found and 0 added, and from 0
of 51 frozen crates to 51 (5.47 s and 5.55 s) (**read**).

The wall time stays because what came from the store is of no use to the first edit: once the
cold build has answered, the worker compiles all it took of the checkout again ("compiling what
the store had of the checkout, for the first edit to patch", 73,586 functions in 8.0 s on
nushell), and the `sync` that `cargo ra-jit build` sends when the worker is ready waits for it:
it arrived as that build began and was answered 9.4 s later. A restart that found nothing
compiled the checkout with marks the first time and had no such build (**read**, the
`request: sync` lines in the worker's log). So the next lever is the one below: marks from
stable names, so nothing is rebuilt for the first edit (**unverified** that it removes those
9 s whole).

## What is left

- "store read" is still 129 s of thread time in helix's "again". The 64 threads ask for the same
  callees together, and each set of types is interned type by type (**read**). How much of the
  build's 16.4 s it still costs was not measured apart.
- 1,615 functions are "rustc's copy of a function of the checkout" and are not kept, and 24,769
  entries call one of them, so they are compiled again on every start (**read**).
- The rebuild for the first edit (14.0 s on helix) is there because a patch's marks hash
  compiled code that holds salsa ids, which a new process does not share, so the marks cannot be
  stored (**read**, `patch.rs`). Done 2026-10-25, below.

## Marks by stable names (2026-10-25)

dev-aws-5, `warmstart.py`, main at `b26f32ee88` against the stable-marks branch on it
(**read**, each number):

| | main | stable marks |
|---|---|---|
| nushell `restart` build | 30.76 s | 21.41 s |
| nushell `again` build | 30.82 s | 21.52 s |
| nushell `warm` ready | 25.29 s | 15.39 s |
| nushell first edit after `restart` | 0.23 s spliced | 0.30 s spliced |
| ripgrep `restart` build | 5.67 s | 4.63 s |
| ripgrep first edit after `restart` | 0.07 s spliced | 0.08 s spliced |

- An entry of the checkout holds a `stable_mark`: its code, and what it calls and reads by
  stable names, with the checkout's crates named by the content ids they had when the base was
  built. The base finds what each one from the store is an instance of from what its callers
  call now, at each look (**read**, `patch.rs`).
- A closure's stable name holds the index of its expression in its body. ripgrep's body edit
  puts a line at the top of `search`, which renumbers the closure `search` passes to
  `HiArgs::sort`, and so names every instance generic over it anew. The old ones are called by
  nothing, recursive sort functions by themselves only, and are left uncalled (**read**: the
  declines before that, "nothing calls ... as it did").
- An instance compiled as another checkout's crate carries that crate's content id, which can
  differ by path; nushell had one, and it made the whole base untracked before such code was
  looked at for any edit (**read**, "untracked: its crate is not of the checkout").
- What is left of nushell's 21.4 s: load 1.0 s, resolve 3.4 s, compile 12.1 s, link 1.8 s
  (main's steps; **read**). No root of the checkout comes from the store: the daemon's symbol
  table has rustc's libraries, so `roots_from_store` is off ("base from the store: 0 roots"),
  and every start compiles 3,618 functions of ripgrep and 30,040 of nushell (**read**, the
  `compiled` column; that those are the roots is **unverified**).
- Adopting the executable ra-jit linked last as a base, as `Base::of_rustc` adopts rustc's,
  needs the symbols as linked. A function's symbol in a build with nothing of rustc's is
  `ra-jit.{crate id}.{path}`, and the crate's id changes with its contents, so an edit renames
  the symbols of its crate. Generic instances are named per build (**read**, `main.rs`
  `function_named_in`).

## Roots from the store with rustc's libraries linked beside (2026-10-25)

`roots_from_store` was off in every cargo daemon, as rustc's libraries are in its symbol
table. It now asks only that the build is not spliced into rustc's executable. Before is main
after #295 (`stable-marks`), after is the branch; dev-aws-7 (64 vCPUs, nothing else running),
`harness/warmstart.py`, the two interleaved, three rounds of ripgrep and two of nushell:

| | before | after |
|---|---|---|
| ripgrep `restart` build | 5.11, 5.11, 5.06 s | 5.00, 4.90, 4.98 s |
| ripgrep `again` build | 5.01, 4.99, 5.16 s | 4.91, 4.92, 4.84 s |
| ripgrep functions `compiled` at `restart` | 3,618 | 2,377 |
| nushell `restart` build | 25.83, 25.61 s | 26.49, 25.36 s |
| nushell `again` build | 25.75, 26.18 s | 25.84, 25.94 s |
| nushell functions `compiled` at `restart` | 30,039 | 25,843 |

- ripgrep's start is about 0.13 s shorter; nushell's is the same within its noise. Finding the
  stored roots takes 0.28 s on ripgrep (1,396 of 2,560 found) and 0.78 to 0.82 s on nushell
  (4,888 of 12,559), about what compiling the found ones took (**read**, "stored roots").
- Most roots are not found because of what #216 reports: "rustc's copy of a function of the
  checkout", 964 on ripgrep and 5,380 on nushell, and 1,353 and 18,249 entries refused for
  calling them, as before (**read**). With every callee printed on ripgrep, each is a small
  function of the checkout that is not generic, which rustc copied into the crate that calls
  it (its symbol ends with that crate, `...Cs9BsQQwRKzFW_2rg`): `FlagValue::unwrap_switch`
  (69 callers), `Match::start`, `Match::new`, `BinaryDetection::quit_byte`, `Stats::clone`.
  The callers are generic instances (`update`, `map_err`, `sink_*`) and closures, which the
  fresh build points at rustc's copy (**read**). An entry may call that copy only where a
  fresh build would pick it too, so the store needs the rule that picks it, or a stable name
  for rustc's copy that says which build of the checkout it is of (**unverified** which).
- So the cold compile at a nushell restart is still 6,401 functions and 51,413 instances in
  14.4 s, as before (**read**, "cold:").

## Calls of rustc's copies kept in the store (#216, 2026-10-25)

An entry whose function called rustc's copy of a function of the checkout was not kept ("rustc's
copy of a function of the checkout"), and every entry that called it was not found ("an entry
that calls what cannot be used"). The entry now names the function and the copy
(`CachedTarget::Copy`): the build that takes it wants the function compiled and calls the copy,
as `callee` does in a build that compiles the caller. What `callee` decides by is in the names
the entry is kept under (the crate's content and ` checked` in the stable name, the program in
the key) but for whether the program links the copy, which `links` asks, and for a function
that returns a future, which stays refused. Before is main at 820c02e20b, after is the branch;
dev-aws-5 (64 vCPUs, one other lane's tests running beside), `harness/warmstart.py`, the two
interleaved, two rounds each:

| | before | after |
|---|---|---|
| ripgrep `restart` build | 4.68, 4.61 s | 4.33, 4.25 s |
| ripgrep functions compiled at `restart` | 2,375 | 0 |
| ripgrep roots found in the store | 1,397 of 2,560 | 1,743 of 2,560 |
| nushell `restart` build | 22.87, 22.75 s | 19.90, 19.68 s |
| nushell `again` build | 22.86, 22.89 s | 19.53, 19.93 s |
| nushell functions compiled at `restart` | 25,844 | 17 |
| nushell roots found in the store | 4,888 of 12,559 | 7,622 of 12,559 |
| nushell "not kept" / "not found" | 6,736 / 23,629 | 6 / 5 |
| nushell first edit after the restart | 0.33 s | 0.58 s |
| ripgrep first edit after the restart | 0.08 s | 0.29 s |

- Where the nushell restart's 19.9 s go now (**read**, `said-restart.txt`): the spare worker's
  crates 2.3 s and the workspace 1.1 s, rustc's symbols 2.2 s, name resolution 4.3 s (69 def
  maps with a procedural macro are not frozen), then the code: finding the stored roots 3.9 s,
  defining 1.4 s, writing the object 2.6 s, 9.9 s in all, and the link 0.8 s.
- Finding the stored roots reads every entry whole, 155,843 of them, about 58 s summed over the
  cores ("store read"), where the build before read 130,019 while it compiled. The blocks the
  store unpacked lately are 32 MB; with 4 GB the reads take 46.5 s and the restart 19.1 s
  (**read**, one run each), so unpacking a block again is not most of it.
- The first edit after the restart is slower: warmstart's edit adds a line at the top of
  `search` in ripgrep's `crates/core/main.rs`. 18 functions of that file from the store are
  stale, as their lines moved, and the patch compiles 158 instances generic over closures
  written in `search` (`filter_map`, `sort_by`, `attach_timestamps`), which the edit renamed: a
  closure's stable name is the index of its expression in the body, which the added statement
  moves (**read**, a temporary print of what a patch compiles). Main compiled those instances
  at the start, and its patch is 21 functions (**read**): rust-analyzer interns a closure by
  its place among the closures of its body (`ClosureKey` in `hir-ty/src/db.rs`), which the
  added statement leaves, so the instances the base compiled are the patch's (**read**).

## Closures named by their place and their source (2026-10-25)

The stable name of a closure is now its place among the closures of its body, as it is
interned (`ClosureKey::nth`), and the first 8 bytes of the SHA-256 of its source text, where it
was the index of its expression. Before is `store-copies`, after is `closure-names`; dev-aws-5,
warmstart.py, the two interleaved on a quiet host (a second round ran under another lane's
load of 97 and is left out):

| | before | after |
|---|---|---|
| ripgrep first edit after the restart | 0.29, 0.29 s, 176 functions patched | 0.26, 0.26 s, 19 |
| nushell first edit after the restart | 0.57, 0.58 s, 66 functions patched | 0.54, 0.56 s, 17 |
| ripgrep / nushell `restart` build | 4.33 / 20.40 s | 4.41 / 19.81 s |

- The place alone is not enough (**read**, storetracked.py's step "a closure above a
  closure"). A closure's instance name prints the arguments of its parent and not its
  signature or what it captures, so a closure added above another took the other's name. The
  patch found base code for that name and called it with what the new closure takes: the
  program printed nothing. The hash of the source names the two apart; that step now compiles
  11 instances the base has no code for, where the steps that edit a generic function's body
  or a line above a closure compile none.
- The hash is of the closure's text alone, so it moves with neither the file's path nor the
  lines around it, which keeps it content-only as the store's keys are (#226), and it is the
  kind of key item content hashes (#344) would give a closure.
- What is left of the edit is the look for what it made stale: 166 ms on ripgrep ("check" in
  the timeline), where main took 21 ms. Each function from the store in the edited file, and
  what calls it, is compiled again to compare its mark with the store's: 382 functions on
  ripgrep, 411 on nushell ("stats"), where the build that compiled them at its start had them
  at hand (**read**).

## What the nushell restart spends its 20 s on (profile, 2026-10-25)

`perf record -F 499` of the worker through warmstart.py's nushell restart on dev-aws-5 (branch
`closure-names`, release build without frame pointers, so by function only), cut into windows
by the second the samples fall in (**read**):

- Seconds 2 to 9 (the symbols of rustc's libraries, name resolution): SHA-256 5 %, SipHash
  2 %, demangling, the parser; nothing above 6 %.
- Seconds 10 to 12, the stored roots, on every core: `Symbol::intern` 33 %, zstd unpacking
  13 %, `Interned<Told>::new` 3 %, `Search<Listed>::own` 2 %. Reading an entry turns its
  debugger's types back into a set (`TypeSet::from` in `Entries::types`), interning every
  name of every type, which 64 threads ask of one interner, the popular names counted on one
  cache line.
- Seconds 12 to 20, on one or two cores: the entries stitched (`Entries::get`,
  `found_callee`), the debug info written with gimli (its string table's `insert_full` 6 %,
  `calculate_offsets` 6 %), and lld's link. What the build read is freed on another thread
  already (`finish`).

So the next costs are the debugger's types read whole for every entry while the roots are
looked for, which only stitching needs; the debug info of all 155,843 functions written again
when nothing changed; and the link of an executable the target directory already has.

## The first edit after a restart: code from the store told unchanged by its items (2026-10-25)

Each store entry now names the items of the checkout its code comes from, as they were written
when it was compiled: the body it is written in, the bodies whose places it tells (a panic's
location, a `#[track_caller]` declaration) with the line and column each begins on, and the
bodies it mentions a coroutine of, and the bodies that give an `impl Trait` in it its type
with the closures in that type, by a hash of the text of each. After an edit of nothing but
bodies, code from the store whose items in the edited files read as they did is taken as it
was, not compiled again to compare its mark. A closure or function an instance is generic over
is not one of its items: its stable name prints it whole, what a closure captures with it, so
the same name is the same code and a changed one leaves the old code uncalled. Before is main
(`24cb49d6`), after is the branch merged with it; "old" is main before #360 compiled everything
at the start (`2be2c76`). dev-aws-7, warmstart.py, quiet host, main and the branch interleaved,
three nushell pairs in both orders (**read**):

| | old | main | after |
|---|---|---|---|
| ripgrep first edit after the restart | 0.09, 0.10 s | 0.30 to 0.32 s | 0.22, 0.23 s |
| ripgrep: functions asked, "check" | 382 (warm), 25 ms | 382, 210 ms | 17, 114 ms |
| nushell first edit after the restart | 0.37, 0.36 s | 0.57 to 0.68 s | 0.53 to 0.70 s |
| nushell: functions asked, "check" | 156 (warm), 216 ms | 156, 439 to 528 ms | 18, 443 to 530 ms |
| ripgrep / nushell `restart` build | 4.78 / 25.7 s | 4.63 to 4.75 / 20.4 to 21.4 s | 4.72 to 4.94 / 21.4 to 22.2 s |

The nushell restarts differ by which ran first of a pair: main first, main took 21.38 and 21.21 s
and the branch 21.44 and 21.61 s, its compile of the roots 9.7 s where main's took 10.2 s.

- What ripgrep still asks is what changed: the 17 functions below the edit whose panic
  locations moved a line, each patched. They cost what they do because nothing they reach is
  compiled yet: `perf record -g` of the edit (frame pointers) puts 27 % of the daemon's time in
  `inlinable`/`small_body`, the MIR of every callee a recompiled function might inline, each
  on a cold body of another crate (its file parsed, 14 %; inferred, 23 %), and most of the
  rest in the instance's own inference, layouts and macro expansions. The revert after it
  ("back", 0.10 s) finds them warm (**read**).
- nushell asked 105 before the `impl Trait`s were looked through: 87 drop glues of types with
  one in them, whose type a body says and no name does, had no items and were compiled to
  tell (**read**, `RJ_DBG` run). Now it asks 18, but the edit takes as long: the check is the
  edited function itself and the 17 below it, compiled cold, 483 to 509 ms each where main's
  156 asks, warming what they share on 64 threads, left the edited one 295 to 320 ms. The
  revert after it takes 0.26 s (**read**).
- Compiling the store's code of the checkout at the end of the restart instead, as main did
  before #360, was tried: ripgrep's check fell to 26 ms, but the restart took 1.1 s longer and
  the edit then wrote the 8,155 functions it compiled, 0.18 to 0.20 s in all (**read**). The
  rest of the gap is work that only a warm database or a persisted answer saves: which
  callees are small enough to inline, kept by the callee's content, would take the largest
  share (**unverified**).
- A stored mark hashed where the named locals of the code are kept for a debugger, and a full
  build and a later compile of the same code differ there: std's `map_fold` over the
  fixture's closures, whose two zero-sized parameters `f` and `g` have a slot each in the
  full build and none in the later compile, the machine code byte for byte the same. Every
  edit after a restart found such code changed and compiled it again; the mark now leaves
  the slots out with the variables (**read**, storetracked.py under `RA_JIT_CHECK_ALL`). Why
  the two compiles give them slots differently is not known (**unverified**).
