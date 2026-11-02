---
status: plan
summary: The speed of light of a proc-macro call (key, hit, miss to a warm server), the token wire and its transport, the vendored server, the in-process bridge and blaze-expand's bookkeeping, counted from the code, with what was closed and what stands in the way.
date: 2026-10-30
---
# Proc macros: speed of light

2026-10-30: the wasm host (`blaze-macro-wasm`), `blaze-wasi`, rust-analyzer's server
(`proc-macro-srv`, `proc-macro-srv-cli`) and `proc-macro-api`'s client are deleted; every proc macro
runs on `blaze-macro-eval` ([dedup.md](../dedup.md#proc-macros-one-engine-blaze-macro-eval)). What
this says of them is the record of the plan at its date.

Part of [usercode](../usercode.md), linked from [speed-of-light.md](speed-of-light.md). Code:
`crates/blaze-expand/`, `crates/proc-macro-api/`, `crates/proc-macro-srv/`. Design:
[macros.md](macros.md), [macros-native.md](macros-native.md). Nothing here has been measured:
nothing compiles until every lane is done. Counts are from reading the code on 2026-10-30.

## The machine (speed-sol2-usercode-macros)

The figures of [the link lane's speed of light](../link/speed-of-light.md), not yet measured:

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

Added for this file, also estimates: a small allocation from a thread cache, about 20 cycles; a
`setenv` (std's environment write lock, then libc's scan of about 150 entries and a copy of
`name=value`), about 0.13 µs on glibc; a thread spawned and joined with an 8 MiB stack (`mmap`,
guard `mprotect`, `clone`, its name, `futex` join, `munmap`, first-touch faults), about 20 µs; a
wake of the daemon's driver and a context switch each way of a socket round trip, about 5 µs.

**The reference call:** `#[derive(Serialize)]` on a struct of 20 fields, through a warm server
or from the store.
- Input: 300 tokens, 200 of them idents or literals, about 120 distinct symbols, 1.5 KB of text,
  3 span parts. Its columns (`Tree::write_wire`: 8 bytes a node, 8 a span, 4 a symbol, then the
  text) are 300 × 16 + 120 × 4 + 1.5 KB ≈ 6.8 KB.
- Output: 1,500 tokens, 300 distinct symbols, 4 KB of text, about 6 parts: 1,500 × 16 + 300 × 4 +
  4 KB ≈ 29 KB of columns.
- The crate's variables: 40, about 3 KB. The shell's passed through (`Environment::passed`): 100,
  about 6 KB.

## A proc-macro call, end to end (speed-sol2-usercode-macros)

The path: `Kept::expand` (`blaze-expand/src/lib.rs`) → `given` → `RunKey { code, given }` →
`index::find_reads` → on a hit `read`; on a miss `inner.expand` (proc-macro-api
`ProcMacro::expand` → `ProcMacroServerProcess::expand_wire` over a `Claimed` connection of the
pool, `pool.rs`) → `written` → `puts::put`.

### A hit

| step | work | bound |
|---|---|---|
| input columns (`wire::relative`) | 6.8 KB written at 32 B a cycle, 3 part rows rewritten | 240 cycles, 0.07 µs |
| the input hashed (`Given::part`) | 6.8 KB at 6 GB/s | 1.1 µs |
| the other 9 parts hashed | one BLAKE3 compression each, about 100 cycles | 0.26 µs |
| the crate's variables (`Given::env_keyed`) | 3 KB at the SIMD rate, in name order | 0.5 µs |
| `Given::finish` | 16 × 32 bytes | 0.1 µs |
| the store | two dependent lookups (the shape, then the result) | 0.2 µs |
| the output read (`read_wire`) | 29 KB copied and checked canonical, about one cycle a word | 2.3 µs |
| its symbols interned again | 300 × one perfect-hash lookup or one probe, about 40 cycles | 3.4 µs |

Speed of light: about **7.9 µs** for the reference call, plus the traced inputs' checks
([speed-of-light.md](speed-of-light.md), "Finding a kept result"). It allocates the output tree
and the shape the calling query owns, and nothing else.

**Today (after this round).** Each step above is one pass, with two exceptions:
- The crate's variables are hashed by about 80 calls of `Hasher::part` (25 cycles a call plus
  about 2 cycles a byte through one-block compressions): 2,000 + 6,000 cycles, 2.3 µs against
  0.5 µs. That is `blaze-usercode`'s `Given::env_keyed`, the parent's; the fix is the
  `Keyer`-style 8 KiB buffer `index::find` already uses (queue, below).
- The variables are sorted on every key (40 pairs, about 0.9 µs). `base_db::Env` keeps them as
  given; a sorted `Env` would make this a check.

(speed-sol2-usercode, 2026-10-30) Both are now changed in `blaze-usercode/src/lib.rs` (queue
00918, done). `Given::env_keyed` gathers its parts in an 8 KiB stack batch and hands BLAKE3 the
whole batch, which hashes the same bytes. It sorts by a precomputed `u128` of each name's first
16 bytes, a tie going to the bytes, at about 4 cycles a comparison: about 0.35 µs in place of
0.9. Tests: `keyed_variables_batched_hash_as_one_part_each` against one `Hasher::part` per
piece, and the existing zero-allocation budget. A hit is now about 7.9 + 0.35 ≈ 8.25 µs, about
4% over its bound, within 10%. Before:

So a hit is about 9.7 µs, 23% over its bound, all of it in `Given::env_keyed`. What
`blaze-expand` does itself is at its bound: the columns copy, the parts table, the read with its
one allocation (`a_hit_is_keyed_and_read_with_only_its_tree_allocated` holds it to one).

**Before this round** the server path's key and kept value were legacy `FlatTree`s in postcard:
- the key built a `FlatTree` of the input (5 `Vec`s, an `IndexMap` insert per span, a string
  table probe per ident and literal, about 100 cycles a token) and postcard-encoded it as varints:
  about 13 µs;
- the read decoded the postcard (a `String` per text, 300 allocations), built a span table and
  rebuilt the tree through `to_subtree_resolved` (about 40 cycles a token): about 25 µs.

That is about 45 µs, 6× the bound. The wasm path (`blaze-macro-wasm`, `Shapes`) already used the
columns; now both do.

**Corrections to the earlier sections.**
- [macros.md](macros.md), Speed, said a hit "allocates nothing but the decoded expansion" and
  that "the input crosses as the tree's own columns". For a warm server's macros that was not so
  until this round (above). It is now.
- [speed-of-light.md](speed-of-light.md), "A proc-macro expansion", put a hit at 0.7 µs. That
  left out the output's read, whose symbols have to be interned again in this process (3.4 µs for
  the reference output), and the check. It also said "a hit allocates one `Vec` of borrowed
  pairs": `Given::env_keyed`'s list is in the task's frame (`blaze_mem::frame::task`), so it
  takes no heap. And "the input is encoded and then hashed" is now one copy of the columns, 0.07
  µs: hashing them in place would save that copy and nothing else, which is under 1% of a hit.

### A miss to a warm server

The bound is the round trip and one pass over each tree each way, plus what the macro itself
does, which is not counted here:

| step | bound |
|---|---|
| one request and one answer: channel, driver wake, `Effect::Send` and `Effect::Recv`, the server's read and write, two context switches each way | 2 × 5 µs |
| the input written into the frame (the columns copied) | 0.1 µs |
| the server reads the tree into the macro's streams (one `Vec` a group, a symbol clone a token, about 20 cycles a token) | 1.7 µs |
| the server writes the output's columns (`TokenStream::write_wire`, a span pack and a symbol index a token, about 20 cycles) | 8.6 µs |
| the client reads the answer's tree, as a hit does | 5.7 µs |
| the trace record: one `pread` of what this scope appended, its lines parsed in place | 2 µs |
| the kept value (`written`: the output's columns, 29 KB, copied once) | 0.3 µs |

Speed of light: about **28 µs** besides the macro.

**Today**, on top of that:

| what | where | cost |
|---|---|---|
| the passed and the crate's variables sent with every request, 9 KB, then decoded into 140 pairs of `String`s | `WireAsk` (proc-macro-api `wire.rs`), the server's `ExpandMacroWire` | about 7 µs |
| each of the 140 variables set with `set_var` and then set back or removed, under one process-wide lock (`EnvChange`) | proc-macro-srv `lib.rs` | 280 × 0.13 µs = 36 µs on glibc, more on macOS |
| a thread spawned and joined for each expansion | `ProcMacroSrv::expand` | about 20 µs |
| every byte of both frames COBS-stuffed and unstuffed one at a time: the columns are dense in zero bytes, so blocks are 2 or 3 bytes | `transport/postcard.rs`, `encode_into` and `unstuff` | 45 KB × about 7 cycles = 90 µs |
| `getcwd` and two `chdir`, and a `stat` of the library | `EnvChange::apply`, `ProcMacroSrv::expander` | 1.6 µs |

So a miss costs about 180 µs besides the macro, 6× its bound. In order of cost, what stands in
the way:
1. **COBS framing.** Removing it is a protocol change on both ends: a length-prefixed frame
   (or the tree's bytes as a raw section after the message, whose length the message gives) on a
   connection that agreed on `version::WIRE_TREES`, with COBS kept for older peers and the
   toolchain's server (`compare`). The server's loop is `crates/proc-macro-srv-cli/`, which no
   lane owns in [crates.md](../../reference/crates.md) (queue, below).
2. **The environment per expansion.** The server starts with only blaze's fixed variables
   (`warm::SET`), so all 140 differ from its own and are set and undone every time. Diffing
   against the last expansion's set is not exact as the code stands: a macro may change a
   variable it was given, which `EnvChange`'s rollback undoes and a diff would carry into the
   next call. The exact design sends the passed environment once per connection by its content
   ID (`Environment` already interns each by ID), keeps it applied, and sets only the crate's
   variables per call, comparing each against the live value before writing. That changes
   `WireAsk`, the server's loop and `EnvChange` together (queue, below).
3. **A thread per expansion.** It keeps a macro's thread locals from outliving its expansion,
   which is also how rust-analyzer's server works. rustc expands a crate's macros on one thread,
   so a macro's thread locals live across a crate's expansions there. One thread per connection
   and calling crate would match rustc better and cost nothing per call, but it changes what a
   stateful macro sees. Lane usercode decides it (queue, below).

**Changed this round:** the client's `write_tree` (proc-macro-api `wire.rs`). It wrote every
request's trees by rebuilding them token by token through `TopSubtreeBuilder` (about 30 cycles a
token, 2.6 µs for the input). Now a whole tree that passes on no fragment (`fragment_contents`)
is written as its columns lie: one copy, with its parts noted from the bytes (`note_parts`). Every
tree is canonical: `TopSubtreeBuilder` builds it, `read_wire` refuses a tree that isn't, and
`change_every_ast_id` rebuilds when it makes two parts one. So that is what the rebuild wrote.
`a_tree_written_as_it_lies_is_the_rebuilt_one` holds the two paths to the same bytes, the same
parts noted and the same tree read back. A tree that does pass on a fragment still goes through
`write_rebuilt`.

**The pool** (`pool.rs`). `claim` scans the connections, with one `OnceLock` load and an
uncontended lock each: about 20 ns a connection, 0.3 µs for 16. That is 1% of the miss bound, so
it is left as is.

### What `proof` should measure

- **A hit:** allocations per hit, one (the output tree) plus the shape; cycles per hit for the
  reference call, within 1.1 × 7.9 µs once `env_keyed` is fixed; bytes hashed per key, about
  10 KB.
- **A miss:** round trips per expansion (one, plus one per sub-request); system calls per
  expansion on each end (from the trace: today about 300 on the server, mostly `setenv`'s
  environment work in libc, plus the thread's 6); bytes per request besides the trees (9 KB
  today, about 0.3 KB with the environment sent once); the fixed cost besides the macro, within
  1.1 × 28 µs.

## The token-stream bytes (speed-sol2-usercode-macros)

**The format.** tt's own columns (`crates/tt/src/storage/wire.rs`, lane format's): 16 bytes a
token with narrow spans, plus 4 a distinct symbol, plus the text. Each span part is named by
its index in the call's parts. No encoding pass is needed: a tree is written as its block lies
(`write_wire`), and read with one checked pass (`read_wire`) into one allocation. This is the
zero-copy columnar layout that Cap'n Proto and rkyv aim at. The one step a mapped format would
add, reading the columns in place with no tree built, can't apply here: the frontend's tree
holds `Symbol`s of this process's interner, so the text has to be interned again on every read,
and that interning is the floor (3.4 µs for the reference output).

**Per token, today:**

| path | encode | decode | copies of the column bytes |
|---|---|---|---|
| a key (`relative`) | one copy, the part rows rewritten | (none) | 1 |
| a kept value (`written`, `read`) | one copy | one checked pass, aligned in place or copied once into the thread's room | 1 each way |
| a request (`write_tree`, whole tree) | one copy into the frame | the server's `read_wire` and its streams | 3 (frame, `LinkWriter`'s buffer, kernel) plus COBS |
| an answer (`read_tree`) | the server's `write_wire` | `read_wire` where the frame lies | 3 (kernel, `LinkReader`'s buffer, the frame) plus COBS |

The two extra copies a frame takes each way (into `LinkWriter`'s buffer, and out of
`LinkReader`'s into the frame) are 29 KB at 32 B a cycle, about 0.3 µs, 1% of a miss. COBS is the
gap (above).

**The legacy `FlatTree`** (`legacy_protocol/msg/flat.rs`) is taken only for a peer older than
`version::WIRE_TREES`: the toolchain's server that `compare` runs (`BLAZE_PROC_MACRO_COMPARE`, a
test of blaze against rustc's libraries), and a server whose handshake answers an older version.
blaze's own warm servers and every key and kept value use the columns. Its speed doesn't matter
on any build path, so it is left as it is.

**Transports.** `transport/postcard.rs` encodes into one buffer per connection and decodes a
frame in place (`unframe`, `parse` borrowing the tree's bytes). `json.rs` is the legacy JSON
protocol of older peers only.

## The server: `proc-macro-srv` (speed-sol2-usercode-macros)

**Per token operation the macro makes.** The server is rustc's own bridge
(`rustc_proc_macro::bridge::server`): each call the macro makes is its arguments encoded into the
bridge's reused `Buffer` and decoded on the other side, with streams as handles into the server's
`OwnedStore`. Streams are `Arc<Vec<TokenTree>>` (`token_stream.rs`), and an ident's symbol is a
`Symbol` of the `intern` crate, a perfect-hash lookup or one probe. This is rustc's in-process
design, run in a separate process. Per operation it matches rustc's server, apart from the one
`Symbol::intern` per ident or literal a macro makes from text (`intern_symbol`, `from_str`),
which costs what rustc's thread-local interner costs to within a probe. No gap is counted here
beyond the fixed costs of a call (above).

**Loading a library** (`dylib.rs`): once per library, or once per change to it, it costs a
`realpath`, a `stat` and a `dlopen` of the library with `RTLD_NOW`. Each expansion then takes a
`stat` of it and the expanders' lock (`ProcMacroSrv::expander`), 0.4 µs. On Windows the library
is copied first (`ensure_file_with_lock_free_access`) so that it isn't locked.

**Per expansion:** the fixed costs in the miss table above. The environment, the thread and
COBS stand in the way, each with its queue request.

## Native expanders and the in-process bridge (speed-sol2-usercode-macros)

`blaze-expand/src/native/`: `mod.rs` walks blaze's tree straight into the macro's
`TokenStream`. `proc_macro/api.rs` is blaze's `proc_macro`, one text with the wasm guest. `lib.rs`
holds its `sys` over a thread-local `__native::Host`, and `host.rs` answers it. No build path calls
it today ([macros.md](macros.md): the native expanders are removed, and the bridge is kept for
change 20's evaluator). It is counted as that evaluator will use it.

**Per call:** no bytes cross. The three sites are packed and the `Host` is set. The output is
built by `TopSubtreeBuilder`: about 20 cycles a token, 8.6 µs for the reference output, the same
as the server's write. Against the same macro through a warm server, this saves the whole fixed
cost of a miss (180 µs today, 28 µs at its bound) and both trees' passes through the wire.

**Per token:** `Ident` and `Literal` hold their text as an `Rc<str>` (`api.rs`, `Ident::sym`,
`Literal::symbol` and `suffix`). That is an allocation and a copy per ident and literal, in both
directions: 200 for the input and about 900 for the output. At about 30 cycles each, that is
33,000 cycles, 9 µs. rustc's bridge passes a `u32` symbol handle, with no allocation. Each group
is also an `Rc<Vec<TokenTree>>`, which the `proc_macro` API's owned streams need, as rustc's do.

**What stands in the way:** `api.rs` is the same text the wasm guest appends
(`blaze_macro_wasm::GUEST`). A symbol handle needs `sys` functions to intern and to read a
symbol's text in both hosts, the native `Host` and blaze-macro-wasm's `add_host`, and the
guest's half is `speed-sol2-usercode-wasm`'s. It is filed for lane usercode (below), to be done
with that sibling.

## What else an expansion runs (speed-sol2-usercode-macros)

| what | per expansion | verdict |
|---|---|---|
| `compare::comparing` | one thread-local read | at its bound |
| `compare::enabled` | was a `getenv` under std's environment lock, a scan of about 150 entries, about 0.4 µs (5% of a hit, with every worker taking the lock); now one `OnceLock` load | **changed**, at its bound |
| `Instant::now`, three on a miss, two on a hit | about 25 ns each (vDSO) | at its bound |
| `count` (`tally.rs`) | a relaxed add into this thread's block, no shared write | at its bound |
| `Dylib::code` | one `OnceLock` load after the library's first hash | at its bound |
| `puts::put` (a kept miss) | a push onto this thread's list, and `Kept::what`'s `String` | once per kept expansion, under 1% of a miss |
| `verdict.rs` | two relaxed atomics per session | at its bound |
| `closure.rs` | gone: `native/closure.rs` was deleted, so the sort fix-orderkeys was to change there no longer exists, and fix-orderkeys has nothing to change in `blaze-expand` | (none) |

## Against the state of the art (speed-sol2-usercode-macros)

| reference | what it does | blaze |
|---|---|---|
| rustc's in-process bridge | handles into the server's interner, one `Buffer` reused per call, the macro run on every call | **beats** on a hit (no run, 7.9 µs at the bound). The in-process bridge matches it per call, and falls short per ident until symbols are handles. A warm server falls short by a round trip and today's fixed costs |
| rust-analyzer's proc-macro-srv | a process per toolchain, a postcard message per call with a `FlatTree`, an environment set per call, a thread per call, no cache | **beats**: columns instead of a `FlatTree` both ways (one copy instead of a rebuild and varints), and expansions kept. It **matches** on the environment and the thread, which it shares with rust-analyzer: both are queued |
| Cap'n Proto, rkyv | zero-copy columns read in place | **matches** on the format: `write_wire` is the block as it lies, and `read_wire` is one checked pass. Reading in place without building a tree isn't possible while the frontend's symbols are this process's (above). COBS framing **falls short** of it today (queued) |

## Queue (speed-sol2-usercode-macros)

The ids are in the report, `/tmp/blaze-lanes/v2-speed-sol2-usercode-macros.md`.
- usercode: length-prefixed frames on `WIRE_TREES` connections (`transport/postcard.rs`,
  `proc-macro-srv-cli`'s loop).
- usercode: the passed environment once per connection, and only the crate's variables set per
  call (`WireAsk`, `EnvChange`, the server's loop).
- usercode: one expander thread per connection and calling crate instead of one per expansion.
- usercode: symbol handles for `Ident` and `Literal` in `api.rs`, with the wasm sibling.
- usercode: `Given::env_keyed` hashed through a buffer, as `Keyer` hashes.
