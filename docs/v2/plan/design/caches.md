---
status: plan
summary: The inventory of every place blaze keeps a result to avoid work, with the grain of each key, how it is invalidated and bounded, its defect and owner; and every hash function by use. The `domains` script holds each hash domain and each non-BLAKE3 hasher in the tree to a row here.
date: 2026-11-12
---
# Caches: the inventory

Lane cache-audit-1112, read on `int-1112`, 2026-11-12. The rule: AGENTS.md `granular-caching`
(65), with 21, 52, 3, 25, 26 and 51. A row is one cache. **Grain** is the grain of an entry's
*key*: `item` (a function, type, impl, goal, macro call, test), `file`, `unit` (a cargo unit, a
run of user code), `crate`, `program` (a linked image), `checkout`, `process`, or `none` (it
keeps no result). A grain of `crate` or coarser has to name its owner or its argument in the
last cell: `scripts/src/bin/domains.rs` fails otherwise, and fails on a hash domain in `crates/`
that no row under "Domains" lists. Cells come from each cache's module doc and key function; a
row marked † had its doc read and not its producer, and its cells are that doc's claims.

"Store" in *bound* is the store's budget and collector (docs/gc.md); "footprint" is the
daemon's memory ceiling (rule 50). "By key" in *invalidation* means nothing is ever removed
or overwritten: a changed input makes another key.

## The caches

| cache | grain | value, and what its key holds | invalidation | after a restart | bound | defect, owner |
|---|---|---|---|---|---|---|
| `query-memos` | item | every tracked query's result (`blaze-query`): the query and its interned arguments | verified by the edges each run recorded; `lru`, `mem_tag` eviction of values | from `daemon-image` | footprint, `Database::collect` | per-instance MIR memos with `lru = 0`, dropped by hand in `hir/src/evict.rs`: monowalk-1112, records-bodies |
| `daemon-image` | process | the database's memos and edges, the VFS tree, kept parses, the cost history: one file a home | whole, by the code identity of the blaze that wrote it and its checks; `BLAZE_RESTORE_WITHOUT` leaves parts unused | it is the restart | one file | a container, not an entry's key; what replaces a whole-image restore with records: records-restart2 |
| `vfs-tree` | file | files and directories by content ID, the Merkle hash of each directory (`subtree`) | the watcher's journal; a directory's hash by its entries | image, then the journal's tail | †not read | none found |
| `disk-stamps` | file | the content hash of a file outside every view (a toolchain, `/usr/include`), by its path and what `stat` says (length, mtime, ctime, inode, device) | by the stamp, and only a stamp older than `RACY_NS` (2 s) is kept, so no write can share the tick of a kept one (`Stamp::racy`, 2026-11-12) | no (hashed again) | none: `Sharded::intern`, one entry a path and stamp ever seen in the process, never removed | off unix the stamp has no ctime and no inode (`created` stands in), so a rewrite that restores length and mtime is missed there; unbounded; no owner |
| `plan-queries` | file | manifests, config, lock, the plan, the units, std's units, `blaze tree`: queries over each file read (`blaze-units/src/load`) | as `query-memos` | image | as `query-memos` | †none found |
| `kept-plans` | checkout | `Roots::plans`: the last plan of each request shape of a checkout (`serve/job.rs:1889`) | replaced when a job of the shape plans | no | `PLANS_KEPT` a checkout; checkouts never leave `Roots` | keyed by the checkout's absolute path and the request's shape: noopkeep |
| `noop-answers` | checkout | `Roots::answers`: the answer of a build that made nothing, one a request shape | let go when a job of its shape walks | no | `noop::MOST_SHAPES` a checkout | request identity over the whole environment: noopkeep |
| `request-certificates` | checkout | one store record (`Kind::Query`) a request shape and content (`serve/job/certificate.rs`): every input the answer is a function of, by content (the view's root, package directories' Merkle ids with their reading units, variables read, outside files and probes, outputs' stamps), keyed by (relocated shape, view root, blaze); and one slot a (shape, checkout directory) naming the last certificate's key | by key; checked whole (`Certificate::holds`), failing only for six named reasons | yes | store | a quick first check that nothing moved, no entry's key (rule 65): on a miss it names the group that moved and the walk asks each unit's own records; replaces `noop-answers` when wired: certificate2 (docs/v2/plan/design/certificate.md) |
| `kept-front-end` | checkout | `Roots::fronts`: the database of a checkout's last build | by the variables it read (`EnvReads`) and its subject | no; from records | footprint (`job/giving.rs`) | one a checkout; a second checkout of the same sources lowers again: records-restart2 |
| `found-record` | crate | a crate's filed image key and its package's file manifest, under its plan key (plan fields, directory Merkle IDs, dependencies' plan keys, front end code) | by key; the filed key is used only when the serve's own image key equals it | yes | store | a crate's key is its entry's key: records-restart2 |
| `dep-image` | crate | a dependency's lang items, signatures and diagnostic items (its def map is its pages, a record beside it), under its image key (crate inputs, every file of its source root, dependencies' surfaces, front end code) and the values of what it read (`reads` slots) | by key | yes | store | crate-keyed; refused whole for a path under the daemon's home (records-1112e) and for every local crate (`eligible`, records-restart2); a crate whose `include!` reads outside its source root is refused whole (`serve::include_refusal`) where `unit-records` files the same read by value: Unowned 3 |
| `def-map-pages` | crate | a dependency's def map as pages read a name at a time, under the image's key | by key | yes | store | crate-keyed, the one form of a served def map (the image holds no def map part); a module's names written once (layout 3): F2 |
| `item-pages` | crate | a dependency's items, impl and trait items, attribute flags as pages | by key | yes | store | crate-keyed: records-items |
| `macro-defs` | item | a declarative macro's tokens, under the position-free hash of its definition, and a file's index of its items (`hir-expand/src/declarative/stored.rs`, `stored/index.rs`) | by key | yes | store | †new this round, doc read only; its lane is not in `/tmp/round2/ready` |
| `impl-tables` | crate | a dependency's impl tables, frozen and paged (`hir-ty/src/method_resolution/frozen*`) | by key | yes | store | crate-keyed: records-impls |
| `dep-bodies` | crate | every body of a dependency: inference and the borrow check's verdict, one part a crate, under `bodies_key` (the image key, every dependency's check key, the borrow checker's mode) | by key | yes | store | crate-keyed, and deep: a body edit in a crate moves the check key of every crate above it (`blaze-diagnostics/src/cache.rs:154`): records-bodies; Unowned 1 |
| `dep-check-outcome` | crate | a dependency's diagnostics before rendering, under its check key and the lint flags | by key | yes | store | crate-keyed and deep as `dep-bodies`; **unowned**: Unowned 1 |
| `dep-consts` | crate | a dependency constant's value, under its crate's check key and its item key (`hir-ty/src/consteval/deps.rs:51`) | by key | yes | store | a crate's key in every entry's key. Its per-item form is written (`body_store/konst.rs`, the item store's fourth step) and off: an evaluation reads the constant's own runtime MIR, which no fact stands for, so every offer would be refused. The one condition that turns it on, and deletes this row's store: runtime MIR being a step of the item store (restore.md, "One form, a function: the end state"): records-bodies |
| `item-store` | item | a body's inference, MIR and borrow verdict, and, written and not switched on, a constant's value (`body_store/konst.rs`; see `dep-consts`), under its identity and the facts the step read (shapes) | by key | yes | store, `blaze_record::Bounds::STANDARD` shapes a key | the identity is made from the lowered body, so a hit pays the lowering: records-bodies |
| `goal-store` | item | a global trait goal's answer, under its stable form and the facts its evaluation read | by key | yes | store, `blaze_record::Bounds::STANDARD` | none found; the crate key in it is the asking crate's identity (name, cfg, dependencies), not its content |
| `records` | item | no cache of its own: the one form (`blaze-record`) a result is kept in with what it read, base key → read-lists → answer; the key of a list is the base and its slot in the base's chain, of an answer the list's bytes and the digest of each read it names | by key; a list older than the open generation and the one before it is no longer searched | yes | store; `Bounds`: 4 lists a generation, 1024 generations a base, `Placed::Full` past it | no user yet: `goal-store` and `item-store` are ported next (primitive-1112), then `runs`, the request certificate and `hir_def::deps` slots |
| `item-facts` | item | the fact values (`blaze-items`: signature, layout, implements, ...) that `item-store`, `goal-store` and `kernel-entries` are keyed by | as `query-memos` | image | as `query-memos` | †none found |
| `expansions` | item | a proc-macro expansion, under the macro's library, the call's tokens with relative spans, the crate's variables and what the run read (shapes) | by key; `BLAZE_PROC_MACRO_CACHE=check` expands as well | yes | store | the library re-given when a store verdict moves, scope equality by order: reth-1112 |
| `macro-machine-code` | crate | a wasm proc-macro module's machine code, under the module's ID and the engine's code ID (`Kind::Machine`) | by key; loaded only from this daemon's own store | yes | store | a module is the unit Wasmtime compiles: argued, no owner needed |
| `runs` | unit | a build script's or test's result, under its code, what it was given and every input it read (`blaze-usercode/src/index.rs`); a pure script a second time under its instance graph's identity, found before it is compiled | by key; `BLAZE_SCRIPT_CACHE=check`, `BLAZE_PURE=verify`, `BLAZE_ELIDE=verify` run as well | yes | store, a bounded chain of shapes | two keys for one result is by design (the pure key needs no compile); the lookup hashes a whole tree (reth-1112-12): reth-1112 |
| `warm-processes` | process | a proc-macro server or test fork server kept running, by its program's code, sandbox and set variables | ends with its process | no | †not read | †none found |
| `test-passes` | item | a test's pass, under its name, the functions it executed and what it read | by key; a history of at most `LOOKBACK` runs | yes | store; the history only grows | †none found |
| `unit-records` | unit | a unit's outputs and diagnostics, under its record inputs and the values of what its closure read | by key; `BLAZE_UNIT_KEY_DEEP=1` plants the old deep key | yes | store | three keys for one record (narrow, quoted, whole: `serve/unit/compile.rs:505`); dependents keyed by a whole-interface digest; per-item read parts not wired: cutoff-1112 |
| `outputs` | file | a whole output as content-defined chunks, a linked image as a recipe over its base (`Kind::Output`, `Kind::Derived`) | by content | yes | store | none found |
| `doctest-images` | item | a doctest example's and the runner's linked image, under the unit's record inputs less its crate identity, and the example's code | by key | yes | store | †none found |
| `why-ledger` | unit | why each step ran: the last key parts of each subject | appended a build | yes | †not read | †none found |
| `cost-predictor` | unit | what a unit's, a checkout's lowering's and a front end's store last cost | learned each run | image | one row a key | not a result; the lowering's is keyed by the checkout |
| `code` | item | a function's dev and release templates, summaries, folded bodies, thunks, statics' templates, side tables, debug info: each under its instance key (code hash, type-argument facts, settings) and what it builds in (`blaze-codegen/src/query.rs`) | by key | yes | store | †the table in `query.rs` read, not each producer |
| `describes` | crate | what describing an instance gives, under the image key of the instance's home crate and the instance ported | by key | yes | store | a crate's key in every entry's key; not written by real builds; workspace crates have none: monowalk-1112 |
| `kernel-entries` | item | a kernel step's value, under the step's identity and the hashes of its reads (`blaze-kernel`) | by key | yes | store, `MOST_SHAPES` | †none found |
| `devirt-world` | program | what a whole program does with trait objects, each object's digest | †not read | †not read | †not read | whole-program by what it is; not audited: Unowned 5 |
| `fn-cache` | item | `#[blaze::cache]`: a function's results in the built program, under its code ID and arguments | by key | the program's own | the program's | none found |
| `link-state` | program | a link's rows (a function, a static, a span table each) and one digest, patched by each build's delta | by the delta's entries | no; the linked output is a `Derived` recipe | one a program | a container of per-function rows; †`blaze-link` not read |
| `layout-profiles` | program | address maps and profile weights by function identity, in `<target-dir>/layout`; PGO and lint profiles by content | by build id | files | †not read | †none found |
| `fetched` | file | downloaded objects by their BLAKE3, an index from an origin (a pin, a pure step) to its object | by key | yes | store | none found |
| `ext-facts` | crate | an extension's build and each fact it computes, under the extension's key (blaze's code, its sources, its grants) | by key | yes | store | an extension's whole source in every fact's key ("coarser than one identity per fact function", `blaze-ext-host/src/key.rs`); **unowned**: Unowned 6 |
| `lean-proofs` | item | what Lean printed of one module of a proof check (a function's model, a proof file, a theorem's check) that it accepted, beside the module under the build directory's `lean/`, with the compiled module Lean kept (`serve/unit/prove.rs`, `blaze prove`) | by the step's key: the Lean release, the module's text and the keys of the modules it imports (`hir_ty_lean::plan`); a verdict carries a hash of itself and is written only after its compiled module is ordered before it | yes | none: one verdict a module of a build directory, gone with the directory | files in the build directory, not records of the store; a proof file is one module however many theorems it proves, so an edit to one proof runs the file again |
| `profile-history` | process | the profile's parquet history: a log, not a result | appended | files | `BLAZE_PROFILE_CAP_MB` | none found |
| `store` | none | the store's own keys, values, chunks, dictionaries, pins (`blaze-cas`, `blaze-format`) | the collector | yes | budget | pins not covering every record family: storegc; seal groups naming every row of a broken group: sealgroups |
| `remote` | none | transport of records between machines (`blaze-global`, `blaze-pack`, `blaze-hub`) | n/a | n/a | n/a | †not audited |
| `flights` | none | single-flight keys: concurrent askers of one download, layout or compile share it | ends with the flight | no | n/a | not a cache |
| `identity` | none | content IDs of inputs (a file's text, a crate's settings, the code of blaze itself): parts of other rows' keys | n/a | n/a | n/a | n/a |
| `not-keys` | none | literals of the domain form that key nothing: messages, test tools, models, proofs | n/a | n/a | n/a | n/a |
| `unaudited` | none | domains this audit did not place (`blaze-launch-host`, `blaze-engine`, `blaze-thin`, `serve/unit/run/launch.rs`) | | | | to be placed; the list only shrinks |

48 rows. By grain: 12 item, 5 file, 4 unit, 11 crate, 3 program, 4 checkout, 3 process, 6 none.

## Unowned defects

1. **Check keys are deep and a crate each.** `bodies_key` hashes every dependency's check key
   (`blaze-diagnostics/src/cache.rs:154`), and `dep-check-outcome` and `dep-consts` are filed
   under it: one body edit moves every entry of every crate above. Harmless while only
   registry crates are eligible; all-miss the day local crates are (records-restart2's
   `eligible`). The form to move to: a constant under its item key and the fact values its
   evaluation read (as `item-store`), diagnostics a body.
2. (done 2026-11-12) `blaze-premap`, an image of every dependency under one key of the whole
   graph, had no dependent crate and was deleted.
3. **An `include!` outside the source root refuses the crate's image whole**, where
   `unit-records` files the same read by its value. File it in the image's `reads`.
4. **`disk-stamps`** is never trimmed, and off unix its stamp can be restored by a writer (no ctime).
5. **`devirt-world`**, `remote`, `layout-profiles` and the rows marked † were
   not read to their producers.
6. **`ext-facts`**: every fact under its extension's whole source.

## Switches that select a caching path

| variable | what it selects | who removes it, when |
|---|---|---|
| `BLAZE_ITEM_STORE=off` | no item store | records-bodies, once bodies are records; `verify` stays as the differential |
| `BLAZE_UNIT_KEY_DEEP=1` | the old deep unit key, as a planted bug in shipped code | cutoff-1112, when revision cases have their in-process driver (`tests/revisions/main.rs` names `sim`; there is no `sim.rs`): a plant is then a value the test gives the job, and no binary reads it |
| `BLAZE_FRONT_END_PER_PLAN=1` | the plan as the kept front end's identity, a planted bug as above | onefront, as above |
| `BLAZE_DEPS=full` | every dependency checked whole, not only what programs reach | stays: the differential run's (`harness/v2/reachable.py`) |
| `BLAZE_DEP_IMAGES` (`check`, `verify`), `BLAZE_PROC_MACRO_CACHE=check`, `BLAZE_SCRIPT_CACHE=check`, `BLAZE_PURE=verify`, `BLAZE_ELIDE=verify`, `BLAZE_ITEM_STORE=verify` | a hit does the work as well and compares | stay: they select no second path |
| `BLAZE_RESTORE_WITHOUT` | parts of a good image left unused | stays: for finding which part a fault follows |

Every other `BLAZE_` variable a file of `crates/` names outside tests selects no caching path
(the `domains` script fails on one that is neither here nor in the table; the code-changing
knobs, `BLAZE_OPT` and the rest, are refused by a build whose keys do not hold them:
`hir-ty-codegen/v2/backend.rs` `CHANGES_CODE`). Other variables: `BLAZE_AHEAD`, `BLAZE_ALLOC_TRACE`, `BLAZE_API_TOKEN`, `BLAZE_ASSEMBLER`, `BLAZE_BACKEND`, `BLAZE_BENCH_MACHINE`, `BLAZE_BIN`, `BLAZE_BORROWCK`, `BLAZE_BORROWCK_DEBUG`, `BLAZE_BUILT_IN`, `BLAZE_CACHE`, `BLAZE_CHECK_PLACES`, `BLAZE_CODEGEN_CODE`, `BLAZE_CORES`, `BLAZE_DAEMON_MEMORY_GB`, `BLAZE_DEV_TOOLCHAIN_AS`, `BLAZE_DUCKDB`, `BLAZE_DUMP_CLIF`, `BLAZE_DUMP_LLVM`, `BLAZE_DUMP_MIR`, `BLAZE_DUMP_TIER`, `BLAZE_DURABILITY_CENSUS`, `BLAZE_EMIT`, `BLAZE_FN_CACHE_TRACE`, `BLAZE_FRONT_END_CODE`, `BLAZE_FUZZ_RUNS`, `BLAZE_GC_TEST_HOLD_LEASE`, `BLAZE_GIT_DATE`, `BLAZE_GIT_SHA`, `BLAZE_GPU_MSL_HARNESS_NATIVE_SHADERS`, `BLAZE_HASH_ORDER_CHILD`, `BLAZE_HISTORY_LEVEL`, `BLAZE_HOME`, `BLAZE_HUGE_PAGES`, `BLAZE_IGNORE_PIN_FOR`, `BLAZE_INLINE`, `BLAZE_INLINE_ASM`, `BLAZE_INPROC`, `BLAZE_LAUNCH`, `BLAZE_LAUNCHED`, `BLAZE_LAYOUTS`, `BLAZE_LEAN`, `BLAZE_LEAN_PROVER`, `BLAZE_LIBRARY`, `BLAZE_LIBRARY_FROM`, `BLAZE_LLVM_INLINE_THRESHOLD`, `BLAZE_LOCK`, `BLAZE_MACHINE_MEMORY_GB`, `BLAZE_MEMORY_BUDGET`, `BLAZE_MIR_OPT`, `BLAZE_NO_HOME`, `BLAZE_OPT`, `BLAZE_ORACLE_MINUTES`, `BLAZE_ORACLE_OUT`, `BLAZE_PINNED_BY`, `BLAZE_PROFILE`, `BLAZE_PROFILE_CAP_MB`, `BLAZE_PROFILE_DETAIL`, `BLAZE_PROMOTE`, `BLAZE_RECORD_CODE`, `BLAZE_REGALLOC`, `BLAZE_RELEASE`, `BLAZE_RELEASES`, `BLAZE_REVISIONS_SEED`, `BLAZE_REVISIONS_STORE_CHECK`, `BLAZE_RUN_AS_DEFAULT`, `BLAZE_RUST_DIST`, `BLAZE_SANDBOX`, `BLAZE_SANDBOX_TEST_ALONE`, `BLAZE_SCRIPT_RUSTC`, `BLAZE_STATS`, `BLAZE_TEST_UNSET`, `BLAZE_TOOLCHAINS`, `BLAZE_TRACE_HOLDERS`, `BLAZE_UI`, `BLAZE_UI_TRANSCRIPT`, `BLAZE_VERIFY_STORE`, `BLAZE_VERSION`, `BLAZE_VERSION_LINE`, `BLAZE_WARM_SOCKET`.

## Hash functions

The rule: a hash that stands for content (in a key, a record, a persisted file, or wherever
equal hashes are taken as equal things with no compare after) is BLAKE3 under a domain
(`blaze_ids::Hasher`), at least 128 bits kept. Classes: `identity`; `probe` (a slot or shard
hash, the full key compared after); `required` (another tool's format); `integrity` (a
checksum of bytes already named); `random` (a seed wanted random, nothing kept by it);
`names` (code that names a hasher and hashes nothing with it). The `domains` script fails on
a file under `crates/` that names SipHash, `DefaultHasher`, `RandomState`, `FxHasher`,
`AHasher` or FNV's basis outside tests and is under no path of this table.

| where | function | class | why, and what a collision costs |
|---|---|---|---|
| `crates/blaze-ids/` | BLAKE3, 256 bits (`ContentId`) or its first 128 (`Fingerprint`) | identity | the rule; also `syntax-tree`'s `subtree_hash` and, since 2026-11-12, `hir-ty-codegen`'s `print_of` |
| `crates/blaze-hash/` | foldhash under one fixed seed; SipHash-1-3 under a drawn key for keys an attacker picks; the key's own first word for keys that are hashes | probe | in memory only; the map compares the key |
| `crates/blaze-std/src/lib.rs` | the above, re-exported | names | |
| `crates/hir-def/src/nameres/frozen/pages.rs` | FNV-1a, 64 bits | probe | persisted slot hash of def-map and item pages; the name or key bytes are compared after (`pages.rs:630`, `items.rs:313`); the layout word is `fnv(ENCODING)`, which does not name the function: a change of it has to bump `ENCODING` by hand |
| `crates/hir-ty/src/method_resolution/frozen/pages.rs` | FNV-1a, 64 bits | probe | as above; the entry is decoded and compared (line 451) |
| `crates/hir-ty-codegen/type_id.rs` | `StableSipHasher128` | required | a `TypeId` as rustc computes it: rustc's code compares it with ours |
| `crates/blaze-units/src/load/plan/hash.rs` | `StableSipHasher128` | required | cargo's `compute_metadata`, input for input |
| `crates/blaze-source/src/source_id.rs` | `StableSipHasher128` | required | †cargo's hash of a source, in registry directory names |
| `crates/blaze-graph/src/ledger.rs` | FNV-1a, 64 bits | integrity | the machine ledger's checksum; also a home's label (`serve/machine.rs:167`), where two homes of one hash and one path tail read as one: †not traced to a decision |
| `crates/blaze-profile/src/flight/postmortem.rs` | FNV-1a, 64 bits | identity | †a layout word of the flight recorder: a collision reads another layout's bytes; no build depends on it |
| `crates/blaze-profile/src/panics.rs` | FNV-1a, 64 bits | probe | a panic site's counter, no compare after: a collision adds two sites' counts |
| `crates/blaze-std/src/rt.rs` | std's `RandomState` | random | a seed from the OS |
| `crates/ix-prof/src/lib.rs` | std's `RandomState` | random | a session's nonce |
| `crates/blaze-query/src/db/value.rs` | | names | `RandomState` in a list of types |
| `crates/blaze-lints/` | | names | the lints that ban these hashers |
| `crates/hir-ty-codegen/fncache.rs` | | names | what `#[blaze::cache]` knows of std's `RandomState` |
| `crates/hir-ty-pure/` | | names | the purity model of `RandomState` |
| `crates/harness/` | `DefaultHasher`, FNV-1a | unaudited | the harness app, not the compiler |

Not held by the script, required by others: SHA-256 for crates.io checksums, lock files and
toolchain pins (`blaze-fetch/src/digest.rs`); SHA-1 for git objects (`blaze-hub/src/git.rs`);
MD5 and SHA-1 where a debug-info format asks; CRC32 for `.gnu_debuglink`
(`blaze-link/src/beside/crc.rs`). Integrity: CRC32C of the image's and the store's page groups
(`blaze-ids/src/crc32c.rs`). 64-bit values used as names, each a probe or a first check:
`item_scope.rs` `name_hash` (in memory, of a pointer), `hir/persist.rs` `text_hash` (keyed at
random; the bytes written are the same under any key), `delta/state.rs` `cost_key` (the first
eight bytes of a content ID, a cost estimate's key).

## Domains

Each hash domain written in `crates/` outside tests, less its version number, under the cache
it keys. `blaze v2 ` is left off; a name that starts otherwise is written with `=`. A literal
with a backtick in it is no domain and is not listed.

- `query-memos`: `db input absent`, `db input config`, `db input file`, `db input names`, `db input path`, `db input root path`, `db shared value`, `db value`, `query key`
- `daemon-image`: `db blob 2: values at 16-byte boundaries, keys' bytes, paths`, `db edges 2: edge(kind u16, pad 6, input 32, saw 16) 56; a memo's in the order read`, `db table 2: u64 1 + a key's first memo or 0; a power of two, probed linearly`, `image spine`, `vfs blobs 1: file bytes`, `vfs disks 2: disk.rs's encoding`, `vfs image`
- `vfs-tree`: `db memory directory`, `db memory root`, `db package subtree`, `db subtree`, `db subtree key`, `db subtree query`, `vfs directory`, `vfs file`, `vfs journal record`, `vfs name index key`
- `disk-stamps`: `disk stamp`, `disk view id stamp`
- `plan-queries`: `cargo asked`, `cargo asked key`, `cargo asked or not`, `cargo config`, `cargo config key`, `cargo config read`, `cargo configured`, `cargo dir`, `cargo dir key`, `cargo file`, `cargo file key`, `cargo fixed dir`, `cargo fixed dir key`, `cargo fixed file`, `cargo fixed file key`, `cargo foreign`, `cargo foreign key`, `cargo found dir`, `cargo found file`, `cargo invocation`, `cargo invocation key`, `cargo manifest`, `cargo manifest key`, `cargo manifest parsed`, `cargo mount index`, `cargo mount table`, `cargo mounts`, `cargo mounts key`, `cargo plan`, `cargo plan key`, `cargo planned`, `cargo std key`, `cargo std placement`, `cargo std planned`, `cargo std units`, `cargo tree`, `cargo tree key`, `cargo tree printed`, `cargo units`, `cargo workspace`, `cargo workspace key`, `cargo workspace read`, `=blaze workspace build dir`, `=blaze-resolve package id`, `=blaze-source interned string`, `=blaze-source registry name`, `=blaze-source source id`
- `kept-plans`: `serve last plan`
- `noop-answers`: `serve no-op config`, `serve no-op drawn`, `serve no-op inputs`, `serve no-op request`, `serve no-op subject`, `serve request shape`
- `request-certificates`: `serve request certificate`, `serve request certificate slot`
- `kept-front-end`: `front end subject`, `front end variable`
- `found-record`: `found key`, `found key directory`, `found record`
- `dep-image`: `=blaze frozen def map`, `dependency image`, `dependency image key`, `dependency image reads`, `dependency signature key`, `dependency signature record`, `dependency signatures`, `frozen print`
- `def-map-pages`: `def map pages`
- `item-pages`: `item pages`, `item syntax`
- `macro-defs`: `file entry set`, `macro def tokens`, `file index`, `item attrs`, `item body`, `item header`
- `impl-tables`: `=blaze frozen impls`, `candidate set`, `impl set`, `impl tables`
- `dep-bodies`: `dependency bodies`, `dependency body`, `dependency runtime MIR`, `hir-def body tree`, `ported ids and types`
- `dep-check-outcome`: `dependency check outcome`
- `dep-consts`: `dependency constant value`
- `item-store`: `head identity`, `impl identity`, `item identity`, `item identity crate inputs`, `item identity lang items`, `item presentation`, `item store`, `item store 1 answer record`, `item store 1 body`, `item store MIR`, `item store borrow check passed`, `item store inference`, `item store step value 1 MIR`, `item store step value 1 inference`, `item-hash absent`, `item-hash attrs`, `item-hash crate root`, `item-hash fields`, `item-hash match check`, `item-hash traits in scope`, `item-hash whole scope`, `item early identity`, `item store constant value`, `item-hash crate names`
- `goal-store`: `goal`, `goal 3 solver`, `goal crate`
- `records`: `record answer`, `record list`
- `item-facts`: `=blaze fact relied`, `bound derive method`, `bound hash`, `builtin derive signature`, `closure code hash`, `code hash`, `contract clause`, `contract fact`, `contract facts`, `coroutine code hash`, `crate key`, `deep hash`, `fact`, `fact auto traits`, `fact captures`, `fact code`, `fact coherence`, `fact const value`, `fact const value type`, `fact crate`, `fact derive`, `fact drop check`, `fact field`, `fact hidden types`, `fact implements`, `fact layout`, `fact method`, `fact name`, `fact resolves`, `fact rpitit types`, `fact signature`, `fact specializes`, `fact target`, `fact vtable`, `field order`, `field order plan`, `form`, `form spans`, `index impls`, `index scope`, `interface hash`, `item key`, `layout`, `layout hash`, `name`, `seen impls`, `signature hash`, `simplified type`
- `expansions`: `expansion reads gone`, `macro-eval code`, `passed environment`, `proc-macro checked expansion`, `proc-macro library`, `verified expansion values`
- `macro-machine-code`: `wasm engine bytes`, `wasm proc macro machine code`, `wasm proc macro module`
- `runs`: `copy origin`, `provider seed`, `pure script identity`, `pure script instance`, `root path`, `run depends on its roots`, `run file shown`, `run given`, `run given part`, `run shape slot`, `run traced`, `shape id`, `test attributes`, `test history slot`, `test identity`, `test members`, `test reached set`, `traced listing`, `unverified copies`
- `warm-processes`: `warm key`
- `test-passes`: `testsel history slot`, `testsel identity`, `testsel set`, `testsel sim beside`, `testsel sim bound`, `testsel sim given`, `testsel unlogged`
- `unit-records`: `cargo test natives`, `cargo test spans`, `cargo unit check`, `cargo unit identity`, `cargo unit identity part`, `cargo unit narrow built`, `cargo unit narrow checked`, `cargo unit narrow code`, `cargo unit narrow interface`, `cargo unit narrow surface`, `cargo unit record`, `cargo unit record here`, `cargo unit record parts`, `cargo unit record quoted`, `cargo unit record quotes`, `cargo unit record reads`, `crate code identity`, `crate own code`, `crate own narrow`, `crate surface slot`, `diagnostics emitted`, `file code text`, `file interface text`, `file still text`, `file surface text`, `library crate narrow`, `test context`
- `outputs`: `cargo linked output`, `cargo output`
- `doctest-images`: `doctest image`, `doctest image without the unit's identity`
- `why-ledger`: `serve job diagnostic said`, `serve settling subject`, `serve unfinished subject`, `why ledger version 4, records zstd`, `why record part`, `why run key`, `why run key with reads`, `why run read`, `why unit subject`
- `cost-predictor`: `kept front end store cost`, `lowering cost`, `program build`, `unit cost`
- `code`: `=blaze asm`, `=blaze global asm`, `allocator`, `allocator item`, `asm`, `asm item`, `behaviour identity`, `behaviour identity leaf`, `body code`, `built function entry`, `built id`, `built in`, `built static entry`, `built static key`, `cached function`, `code`, `code id`, `code identity inlined`, `code identity name`, `code identity value edges (test)`, `code with names read`, `codegen binding names`, `codegen discriminant`, `codegen names read`, `codegen owned code`, `codegen package name`, `codegen policy`, `codegen settings`, `codegen span table`, `codegen template code`, `codegen told types`, `compile identity`, `compile identity callee`, `compile identity value`, `compile identity walk`, `compiled code print`, `counter`, `data`, `debug name`, `edits fixture code`, `exec bound`, `exec fn id`, `exec placed`, `exec span table`, `exec static`, `exec static value`, `fold data`, `fold ir`, `fold references`, `fold shape`, `folded`, `folded identity`, `function digest`, `global_asm object`, `group flight`, `hir-firewall code`, `identity adt`, `identity args`, `identity bound`, `identity const`, `identity type`, `imports`, `instance`, `instance abi`, `instance bound`, `instance identity`, `link span table`, `live assumed unhashable`, `masked`, `multiversion plan`, `own place shape`, `owned data`, `placed`, `placed args`, `recursion shim body`, `reference placed`, `reference placement`, `relative flight`, `relative shape`, `release imported bodies`, `release laid out`, `renamed owned data`, `shape`, `share fact`, `shared args`, `shim`, `side`, `start`, `static definition`, `static digest`, `static value`, `store folded`, `store masked`, `store release template`, `store side`, `store static`, `store summary`, `store thunk`, `subject`, `summarized`, `target`, `template`, `template own span table`, `test listing`, `told args`, `toolchain id`, `why dev template step`
- `describes`: `describe record`, `describe record stage`
- `kernel-entries`: `kernel entry`, `kernel identity`, `kernel shape slot`
- `devirt-world`: `devirt code`, `devirt fixture`, `devirt object`, `devirt object args`, `devirt site`, `devirt world`
- `fn-cache`: `cached code`
- `link-state`: `link asan global name`, `link asan globals`, `link asan module name`, `link asan odr indicator`, `link cie`, `link debug record`, `link file`, `link file block`, `link file name`, `link file node`, `link function`, `link given`, `link given entry`, `link image`, `link keyed leaf`, `link keyed node`, `link keyed set`, `link lineage`, `link msan option`, `link sancov bound`, `link sanitizer constructor`, `link sanitizer constructor pointer`, `link shipped native input`, `link static`, `link target autolinked libraries aarch64`, `link target autolinked libraries x86_64`, `link target default libraries`, `link target re-exported libraries aarch64`, `link target re-exported libraries x86_64`, `link target script`, `serve debug side file`, `serve debug side plist`, `serve launch sidecar`, `span table`
- `layout-profiles`: `args`, `layout image`, `layout key`, `layout weights`, `link heat`, `link layout`, `lint profile`, `perf code`, `perf component`, `perf deep`, `perf image`, `perf merged`, `perf rep`, `perf run`, `pgo name`, `pgo profile`
- `fetched`: `=blaze pack chunk`, `=blaze pack object`, `fetched package on disk`, `=blaze-fetch`, `=blaze-fetch chunk: a piece of an object by its BLAKE3`, `=blaze-fetch index record: form, BLAKE3, length`, `=blaze-fetch object as an input`, `=blaze-fetch object: bytes, or chunks of 4194304 bytes, by their BLAKE3`, `=blaze-fetch origin`
- `ext-facts`: `extension body`, `extension build`, `extension fact`, `extension fact last`, `extension fact value`
- `lean-proofs`: `lean step`, `lean verdict`
- `store`: `=blaze gc lease nonce`, `buffer`, `hashcons pack`, `schema`, `store chunk`, `store cold unit`, `store dictionary`, `store key`, `store pin`, `store staging`, `store value`, `zstd dictionary`, `store set leaf`, `store set root`
- `remote`: `=blaze hub tree edge`, `=blaze hub tree node`, `=blaze pack manifest`, `=blaze pack signed message`, `global bundle`, `global manifest`, `global manifest key`, `global piece`, `global record key`
- `flights`: `serve fetch`, `serve library`, `serve library fingerprint`, `serve native`
- `identity`: `=blaze error union member`, `=blaze shipped code identity`, `build id`, `cfg options`, `codegen code`, `codegen code input`, `crate`, `crate graph`, `crate ported`, `crate ported files`, `crate setting part`, `crate workspace data`, `env`, `file text`, `file without text`, `front end code`, `included file`, `named paths bucket`, `record makers`, `shared store writer`, `source root`, `source root content without other crates' OUT_DIRs`, `source root leaf`, `source root node`, `source root path`, `target data`, `unit record code`
- `not-keys`: `=blaze fetch-url's request is not UTF`, `=blaze storemodel code`, `=blaze storemodel crash check`, `=blaze storemodel disk`, `=blaze storemodel expectation`, `=blaze storemodel fact`, `=blaze storemodel input`, `=blaze storemodel key`, `ci oracle sample`, `proof kernel read`, `proof kernel value`, `verify store sample`, `=blaze-bench-protocol`
- `unaudited`: `=blaze launch file name`, `=blaze launch location`, `=blaze launch program`, `=blaze pin lookup`, `launch slot`, `rustc stand-in library directory`, `rustc stand-in library root`, `rustc stand-in probe view`
