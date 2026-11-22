---
status: plan
summary: How blaze obtains the sources of a project's dependencies itself (crates/blaze-fetch). A Cargo.lock becomes a plan of units (a crates.io crate pinned by its SHA-256, a git commit pinned by its id); a sans-IO machine asks for each over HTTPS, checks it before a byte is unpacked, and asks the store to keep one content-addressed record of its files. Covers the plan, the machine, the trust steps, the record and its key, what the driver's Fetch task calls, the first numbers on tempo's lock, and what is not built.
date: 2026-11-16
---
# Fetching dependency sources

## Plan

`blaze_fetch::plan::plan(&Lock)` reads the one lock reader (`blaze_manifest::lock`) and makes
one unit per thing to fetch:

- **Crate**: a crates.io package with a `checksum`: `name`, `version`, the SHA-256.
- **Git**: one commit of one `https://` repository, however many packages it holds (tempo's
  reth commit holds 110). The commit is the lock's `#fragment`, 40 hex digits; a branch or tag
  alone is never fetched.

A package that cannot be verified is named in `Skip`, never fetched and trusted: a registry
package with no checksum, another registry's package, a git source that is not `https://`.
Whatever becomes a URL is checked again here (name, version, repository characters, no `..`,
`@` or `?`), whatever the lock reader allowed.

## Machine

`blaze_fetch::Machine` (sans-IO) takes units and gives effects: `Lookup { key }`,
`Http { url, method, headers, body, most, deadline }`, `Put { key, record }`,
`Reserve { key, room }`. Its completions are `looked`, `answered`, `reserved`, `stored`; the time
comes with each call. `Machine::new` wants every unit (a whole lock); `Machine::wanting` wants
none until `want(unit)` names one, so one machine a command serves every `Fetch` task that runs,
under one budget, and a command whose tasks all stand asks nothing. A unit goes: lookup (held:
`Present`, nothing asked) → request(s) → checks → unpack → put → `Fetched`. At most
`in_flight_most` requests at once; a request past its deadline is failed by the machine and its
late answer ignored. Every request is tried ONCE (D172, rule 3): no connection, a broken one,
no answer in time, a 429, a 5xx or a wrong digest fails the unit with a typed `Why` at once,
named with its source. (Until 2026-11-20 the first four were asked again after a doubling
backoff, `tries_most` tries in all.) An answer past a first request's `most` is asked again
sized to its length: a bigger buffer, not a second try.

THE RULE FOR A FAILURE OUTSIDE THE CHECKOUT, stated once here: a task that failed for a reason
outside the checkout (the network, a missing source, the store) is OWED again on every command
until it succeeds, and the profile says how many such tasks a command retried. Its failure is
never a record that stands: the `Fetch` (and the graph or `Units` that failed for it) goes on
the Units phase's owed list (blaze-cli engine_units.rs `Discovered::owed`, engine_check.rs), so
the command's certificate does not stand and the next command runs the task again: a source
put in cargo's cache or the store since, or a network back, is fetched then. The plan line
counts them (`owed again N`). A failure of the bytes (a wrong digest, a hostile archive) is
lasting and stands by its key: the same key fails the same way.

A git commit takes two requests: the protocol v2 advertisement (`info/refs`), then `fetch` of the
one commit at depth 1 (blaze-git-fetch's sans-IO pieces, carried by the same HTTPS client).

## Trust steps, in order

1. The pin is the lock's: the SHA-256 for a crate, the commit id for git.
2. The answer's length is held to `most` by the driver, and again by the machine.
3. A `.crate`'s SHA-256 is compared with the lock's before a byte is inflated. A mismatch names
   the crate and both digests. A git pack's trailer SHA-1 and every object's id are computed
   from their bytes, and the tree is walked from the pinned commit, so every file is bound to it.
4. Inflate (blaze-deflate) is bounded in output and in ratio before it writes; the tar reader
   (blaze-tar) checks checksums, refuses absolute paths, `..`, devices, links out and sizes past
   its limits.
5. Every entry lies under `name-version/` (stripped); a hard link is refused; a symbolic link is
   kept only when its target stays inside the folder. A size a header declares is held to what is
   left of the limit before anything is reserved for it.
6. The record's paths follow the listing rules of `tree` (no `.git`, no two names one on a
   case- and normalization-insensitive disk, no path under a file).
7. Only then is a record made, and only a made record is put, whole or not at all.

## Record and key

Key: `blake3_derive("blaze-fetch: a registry crate's files, pack v3", name, version, sha256)` or
`(... "a git commit's files, pack v3", commit)`, length-prefixed parts (`blaze_ids::Hasher`), from
the lock's text alone, before any work: `SourceId::key()` (`Unit::key` is the same). A change to
the form is a new domain.

Record `BLZFPK03` (`pack.rs`, little-endian): a 96-byte header (magic, kind 1 crate / 2 git / 3
a toolchain tree, counts, the source digest, the BLAKE3 of the files' `blaze tree 2` listing),
then the data in archive order, then the paths, then 56-byte rows sorted by path (path range,
mode, data range, the file's BLAKE3), and a BLAKE3 of all of it last. Data comes first so every
file is written once, straight into the record (`Builder::append`, `Builder::slot`). Two builders
of one key write equal bytes.

Where it lives (decided by the coordinator, 2026-11-16 05:05; not built yet, drive.md's owed
list): a source record is content-addressed and the same for every project, so it is in the
user's shared store, `<home>/.blaze/v2/sources/<64 hex key>`, beside the toolchain shelf
`<home>/.blaze/v2/toolchain/`, never under the project. A second checkout or worktree of the
same project fetches nothing and writes only its own small records. The home is a passed value
(rule 75), so tests give a temp folder. Two commands may fetch one unit at once: each writes a
temporary name and renames whole bytes onto the key, so both write equal bytes under one name.
A simulation that interleaves two fetchers and crashes one mid-write proves it, with its oracle
(no reader maps a record that fails its digest; no record is lost) and a planted bug (rename
before the bytes are complete) that it catches. A record is verified once a boot, by the
store's boot rule, not on each read.

## Reading a record in place

`PackView::open_source(&SourceId, bytes)` checks the form, every row, the source (kind, SHA-256 or
commit) and the BLAKE3 of every byte, once; `open_source_put` skips the BLAKE3 for a record this
machine fetched and put (verified once per fetch). Then `files()` (sorted, each with `path`,
`mode`, `bytes: &[u8]` borrowed from the mapping, `blake3`) and `find(path)`, with no check again.
`examples/read.rs` measures it.

## The driver's loop and the `Fetch` task

`blaze_fetch::drive::step(machine, io, wait_most)` is the one loop body: it carries out every
effect the machine has through a `FetchIo` (`lookup`, `put`, `send`, `wait`, `keep`, `reserve`)
and waits once for an answer or the next deadline. `examples/fetch.rs` is a `FetchIo` (one file a
record, `blaze_core::reserve` for records made in place, blaze-http's `Handle` with `gzip: false`,
`https_only: true`, the effect's `most` and deadline, `Kind::TooBig { said }` mapped to
`NetFailure::TooBig { said }`, a User-Agent of `blaze/<version>` and nothing else of the user's).
blaze-cli's work.rs is the other, over its store.

`blaze_drive::fetch_task`: one `Fetch` task a plan unit (rule 65: never one for the lock). Its
name's subject and its source are the unit's key, so a repeat stands on its record. Its run wants
its unit of the command's one machine and joins on the outcome; what a reader sees is the key, the
same whether the record was present or fetched now. A failure is one finding naming the unit;
`lasting` (the bytes' fault: a checksum, an archive, git's answer) or not (the network's or the
store's: no record is kept, so the next command asks again). Without blaze-cli's `fetch` feature a
missing record is the finding "this build cannot fetch".

## TLS roots

The client checks servers against roots it is given (rule 75), never a list built in.
`blaze_http::roots::system(Places)` reads the operating system's store when a client first
connects (`blaze_core::trust::system`), and `roots::with_der` takes a configured bundle's roots
(`trust::pem::certificates`), which replace the system's.

- **macOS:** Security.framework and CoreFoundation are opened with `dlopen` on that first read,
  never linked at load. The roots are the system's anchors, minus those the admin or user trust
  settings distrust for SSL, plus those they trust. Where the user and admin settings disagree,
  the user's decision wins.
- **Linux:** `SSL_CERT_FILE`, then `SSL_CERT_DIR`, else the distributions' bundles, then
  `/etc/ssl/certs`. The PEM is decoded with blaze-base64.

No root is `TrustError`, which names every place looked and what each gave. A root rustls refuses
is `TlsSetup::BadRoot`. The old `native-roots` feature (rustls-native-certs, security-framework,
core-foundation, bitflags, and Security linked at load) is gone from blaze-http. blaze-global still
uses rustls-native-certs under its `net` feature.

`trust/linux.rs` has not been compiled yet. The first build of it will be CI's
`cargo check and test, linux arm64` job (`.github/workflows/blaze.yaml`, job `cargo`).
`Effect::Roots` (the effect form, for bstd's `Io::roots` and the simulation's world) is the
effect lane's to wire after Boot. The hunks are in that lane's hands.

## Memory

Unpacking streams: a `.crate` is inflated 16 KiB at a time into the tar reader and each file is
appended to the record, sized once from the gzip trailer's ISIZE, which is only a hint held to
min(the inflate limit, archive length x ratio limit) before the reserve; a trailer that disagrees
with the inflated length is `Gzip(Why::Size)` and nothing is stored. A git pack is indexed in place
(`blaze_git_fetch::Index`: offset, kind, size, id per object; each object's id checked against its
bytes before use, also late in a delta chain), the tree listed, the record sized exactly, and each
blob resolved straight into its slot through a base cache of at most 32 MiB.

Bytes in flight (`Limits::bytes_most`, 512 MiB): every request's `most` and every record not yet
stored. A first request asks 1 MiB (`.crate`) or 32 MiB (git pack); past it, it is asked once
more sized to the stated length, or with the limit held to the budget when none is stated.

## Proof

`tests/sim.rs` (blaze-sim): random crates, some hostile (an entry beside the folder, a gzip bomb,
a header claiming 4 GiB, a gzip trailer lying high or low), a store holding some, an origin whose
every answer is on time, late, lost, refused, 503, 404, cut short, flipped, another crate's,
tampered, past the request's `most` (with its length stated, unstated or a lie) or too big; a crash
at any effect, then a calm run, then one that must ask nothing more than the hostile crates. Oracles:
every stored record is its truth, no hostile crate is stored, no allocation passes 4 MiB, the bytes
in flight stay within the budget's bound, every run ends. Planted bugs caught: a pack stored before
its checksum, an entry outside its folder kept, a size from the wire (tar header or gzip trailer)
reserved unchecked, a commit taken from the server's pack instead of the lock's id. A third of the
units are git commits served as GitHub serves protocol v2 (advertisement, then a depth-1 answer
with no length stated); hostile ones carry a tree entry `..` or an 8 MiB blob packed small, and
the fates add another commit's answer and a blob changed under its tree's id. A git answer past
what the budget lets it ask for must fail by name (`TooBig`, no length stated). blaze-git-fetch's
own tests catch an object taken by the nearest id, an unchecked trailer and a corrupted object
deep in a delta chain (their plants are that crate's own).

## Numbers (tempo's lock, a Mac, 2026-11-16)

1348 packages: 1169 crates and 4 git commits, 52 by path, none skipped; 187.5 MiB down, 59,190
files, 1191.7 MiB unpacked. Measured with a counting allocator per phase and macOS's lifetime peak
`phys_footprint` (`proc_pid_rusage`).

| | heap high-water | peak | time |
|---|---|---|---|
| before (whole tar, builder copy, record; pack objects held) | 480.2 MiB | 1.97 GB RSS | 5.84 s |
| streamed crate, indexed pack | 157.5 MiB | 551.2 MiB footprint | 8.49 s (network) |
| plus blocks of 1 MiB and up mapped (example's allocator only) | 171.6 MiB | 184.6 MiB footprint | 5.82 s |
| plus the budget | 181.7 MiB | 197.3 MiB footprint | 5.78 s |
| records of 16 KiB and up made in place by the library (`Effect::Reserve`), the loop on `drive::step` | 87.7 MiB (in a put) | 84.8 MiB footprint | 15.49 s (network: 12.1 MiB/s down) |

The same store again (store s15-drive-cold, 2026-11-16): 1,173 present, nothing asked, 0.10 s,
footprint 9.6 MiB; from a fresh copy of the lock, 0.09 s, 9.5 MiB.

What the peak is made of. These are three cold runs on 2026-11-16, each into its own store.
`examples/fetch.rs` snapshots the following at the heap's high-water:
- the answers the loop holds;
- the requests open (sent and not yet taken by the loop, so queued answers count);
- every live block of 1 MiB and up.

| Run | Heap high-water | Peak footprint | At the high-water |
|---|---|---|---|
| Whole lock (s16) | 116.6 MiB | 101.4 MiB | Loop holds 6.7 MiB of answers; 13 requests open |
| Git only (s17) | 60.4 MiB | 52.8 MiB | Loop holds 15.3 MiB (packs); 0 requests open |
| Crates only (s18) | 101.1 MiB | 93.0 MiB | Loop holds 1.3 MiB; 15 requests open; 14 blocks of 1 MiB and up, 92.2 MiB in all: 16, 16, 14.5, 12, 8, 4, 4, 4, 3.1, 3, 3, 2 MiB |

The same lock peaks between 85 and 117 MiB from run to run (the earlier row's 84.8 MiB is one such run).

So the peak is answer bodies in blaze-http and the mailbox: the loop holds little of it. The
powers of two show bodies grown by doubling in blaze-http's driver (`driver.rs`: `body.extend_from_slice`).
Up to half of each such block is spare capacity.

The floor is about 12 MiB: what is live with nothing in flight (7.6 MiB at the end), plus each open
request's read-ahead window (`WINDOW`, 256 KiB, times 15 open requests). An answer is not needed
whole in memory: it can go into its scratch file as it arrives, hashed as it goes, and be unpacked
from that mapping once its digest matches. The ratio is 93.0 / 12, about 8x. The mechanism costing
it is whole bodies in the heap; the end state is a body written straight to a scratch file
(`Effect::Reserve` sized by Content-Length for a `.crate`). It is not built.

The peak is one crate, windows 0.61.3: a 109 MiB record (floor: that record plus what is in
flight). reth's pack: base cache 383 hits of 570 delta reads, at most 1.6 MiB held.

Reading in place (store s08, warm): all 1,173 records opened by id and every `Cargo.toml` (1,344)
read: 28.9 ms with `open_source_put`, 563 ms with `open_source` (the BLAKE3 of 1.2 GB), footprint
5 MiB. windows' 721 files read whole at 22.6 GB/s.

## For a successor

- Work in /Volumes/Projects/blaze-fresh-1115; build only via
  /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh and examplerun.sh. tempo's checkout is
  read-only input. Stores go in a new named folder each run; s08-v3-all is read by the unit graph.
- Done:
  - records made in place by the library (`Effect::Reserve`, from `reserve_from`);
  - one loop (`drive::step`) and one machine a command (`Machine::wanting`);
  - the `Fetch` task's pure half (`blaze_drive::fetch_task`);
  - the git origin in the simulation;
  - the system trust store (`blaze_core::trust`, `blaze_http::roots::system`/`with_der`), with
    `native-roots` deleted.
- The trust tests are green: `lanecheck.sh blaze-core test trust` gave "check ok (all targets)",
  3 passed, 0 failed, on 2026-11-16. The driver lane has been told to swap its `NotReadYet` stub for
  `blaze_http::roots::system(roots::Places { cert_file, cert_dir }) -> Result<Arc<ClientConfig>, TlsSetup>`.
  A configured bundle goes through `trust::pem::certificates` and then
  `roots::with_der(impl Iterator<Item = &[u8]>) -> Result<Arc<ClientConfig>, TlsSetup>`.
- Open, in order:
  1. **The cost to a command that never fetches.** Measure only after the driver's swap. Before it,
     nothing reaches rustls and the optimizer drops the stack: on 2026-11-16 the `fetch` binary
     (7,914,400 bytes, tree 6453171635) linked only libSystem and held no rustls string.
     - Build both sides, two builds in all:
       - `GREENMIX_KEEP_AS=blaze-fetch bash greenmix.sh blaze-cli --release`;
       - `GREENMIX_FEATURES=--no-default-features GREENMIX_KEEP_AS=blaze-nofetch bash greenmix.sh blaze-cli --release`.
     - Quote each run's tree id.
     - Check with `strings` that the with-fetch binary really holds the TLS stack.
     - Then compare: size, bare `blaze` (instructions, page faults), and a repeat `blaze check` on a
       copy of hello world.
     - Check with `otool -L` that Security is not a load-time library.
  2. **Memory's end state.** Each body goes straight into its scratch file as it arrives (an
     `Effect::Reserve` sized by Content-Length for a `.crate`), hashed on the way, and is unpacked
     from the mapping once its digest matches.
     - Target: the floor of about 12 MiB.
     - Baseline: the three runs in Numbers (101.4, 52.8 and 93.0 MiB peak footprint).
  3. **`trust/linux.rs`** has not been compiled. CI's `cargo` job (linux arm64,
     `.github/workflows/blaze.yaml`) will be its first build.
  4. **The first-use counter test.** A program-level test that the store is read on the first
     connection and never at start: the count is 0 after start and 1 after the first fetch.
     Today only `trust::macos::reads()` rising on an explicit call is tested.
  5. **`Effect::Roots`**, which the effect lane wires after Boot.
- Lane ids: read /Volumes/Projects/blaze-int-1112-notes/OWNERS.md, never a brief.

## End state: dependencies on the command path (architect, 2026-11-16)

`blaze check` reads every outside package it reaches from one machine-wide place, verified once,
mapped, shared by every project and worktree; the network is asked only for what no place holds.
**Identity.** A package's sources are keyed from the lock's text before any work (rule 65):
`SourceId::key()`, BLAKE3 under a versioned domain of (name, version, the `.crate`'s SHA-256) for a
registry package, of the 40-hex commit for git. Never a path, a host, a time, a folder name, or the
place the bytes came from. A path dependency is not fetched: its files are the checkout's, keyed by
text like any member. A package the lock cannot pin (no checksum, another registry, non-https git)
is named by `Skip` and stops the command with that name (rule 3).

**Where bytes come from, cheapest first.** Each place is tried in this order; every byte is checked
against the key before it is trusted, and a unit no place gives fails naming every place tried
(`local::Tried`), never skipped and never handed to another tool:
1. The machine's sources store, `<home>/.blaze/v2/sources/<64 hex key>`: one record a package
   (`BLZFPK03`), written once, verified once a boot (the machine store's boot rule), then mapped.
2. Another tool's archive of the same bytes: cargo's `<cargo home>/registry/cache/<crates.io
   folder>/<name>-<version>.crate`. Its SHA-256 is compared with the lock's before one byte is
   inflated, then it is made into the same record at the same key as step 3 would make. Cargo's
   unpacked `registry/src` is never read: no digest covers it. Git: cargo's bare repositories may
   be read the same way later, bound to the pinned commit by walking its tree (piece 14).
3. The registry over HTTPS: `static.crates.io/crates/<name>/<name>-<version>.crate`. The sparse
   index is not needed: the lock already pins name, version and digest, and the download URL is a
   fixed template. Git: protocol v2 `fetch` of the one commit at depth 1.
A path package is read from the checkout. Nothing is copied or unpacked into a folder tree: a file
of a dependency is a slice of its mapped record, so a thousand packages cost a thousand mappings
and no inodes per project. A second checkout or worktree finds every record by key and writes none.

**Writes.** Two commands may make one key at once: each writes a temporary name and renames whole
bytes onto the key (equal bytes, rule 25). A record's presence under its key is the fact: no other
record vouches for it, so a deleted file is made again, never a wedge.
**Per-package records.** Later stages write, per package, an outline, names (`CrateScope`), items,
checked signatures and bodies, keyed by the package's source key plus features, cfg, target and
the toolchain (rule 21). A registry package's records are public (target.md "Namespaces"): they go
to `v2/shared` and CI seals them into the bucket; anyone reads them without a login; a fetched
record is checked against its signed statement, its digest and the size limits before it is mapped
(remote-read.md "Trust"). A package whose records all arrive this way needs no source bytes at all
(deferred, as Buck2 defers materializing): sources are fetched only when a task asks for a file.
Private code (members, path, git, private registries) never leaves `v2/own` or the checkout.

**`blaze check` on three machines.** Never seen the package: the graph task wants each level's
packages at once; each is adopted from cargo's archive or fetched, overlapping up to the in-flight
limit. Seen: every key is a mapped record; when the graph record stands, no source record is even
opened. Offline: the units no place holds fail with the typed finding naming the package and each
place tried; packages that were held are checked; the next command tries again.

**Build scripts and proc macros of dependencies** run sandboxed and are records keyed like their
inputs: build-scripts.md ("Native scripts (D65)") and proc-macros.md own them.

**Cost and floors.** First fetch of tempo's ~1,170 crates: 187.5 MiB down. Floor: (3 + D) round
trips (name lookup, connection, TLS, then one per level of the graph, D about 15 to 20) plus the
archives' bytes over the link; the bytes dominate (15.5 s at the measured 12.1 MiB/s, 2 s at
100 MB/s). Multiplexing on one connection (HTTP/2 or 3, 100 streams) removes per-request waits:
1,000 small requests at 16 in flight and 20 ms cost about 1.25 s of waiting, at 100 streams 0.2 s.
Everything on the machine: one open and map a package, about 10 us: 10 ms on one thread, 2 ms on 8
cores for a cold graph; 0 ms when the graph record stands. Today 28.9 ms for 1,173 records and
every `Cargo.toml` on one thread (ratio about 3). Adopting cargo's archives: one SHA-256 and inflate
a package, once a machine.

**Rules.** Keys from text (65); home, cargo home, roots and clock passed in (75); places in a fixed
order, each verified, every miss named (3, 19); equal bytes (25); private code stays (Namespaces).

**The best systems, and blaze against them.**
- cargo: no index needed, no unpacked copy; compiled results shared across projects (not `target/`).
- pnpm: one copy a machine, matched; no link farm, so a project costs no inodes and no link step.
- uv: warm installs in milliseconds and parallel streaming unpack, matched; the lock resolves.
- Nix: signed statements over keys from text, matched; finer grain: per item, not per package.
- Bazel's repository cache: sha256-keyed, matched; the archive is mapped, never extracted.
- Buck2's deferred materialization: matched; sources fetched only when a task reads a file.

### Gap (from search, 2026-11-16, tree /Volumes/Projects/blaze-fresh-1115)

| Needed | Today, with evidence | Missing | Reached by |
|---|---|---|---|
| Key from the lock | plan.rs `SourceId::key`, `Unit::key` | nothing | the command |
| Machine sources store | fetch.rs `SOURCES` = `.blaze/v2/sources` under the checkout; graph.rs `Mapped`, names.rs `Folder` read it there; store.rs `Sharing` keeps `Fetch` records in the checkout | the home's `v2/sources`; every worktree refetches | the command (checkout form) |
| Another tool's archive | none; blaze-unit-graph sources.rs says "Nothing is read from cargo's caches" | the driver step; `blaze_fetch::local` written 2026-11-16, never compiled | only a test |
| Registry over HTTPS | fetch.rs `net::Net` on blaze-http (HTTP/1.1 pool), `Machine::wanting`, `want_ahead` per level | multiplexing (HTTP/2 or 3) | the command (feature `fetch`) |
| Verified once a boot | fetch.rs `held_record` reads and BLAKE3s the whole record on every `Fetch` run; graph.rs and names.rs use `open_source_put`, trusting the checkout's `Fetch` record | a boot record for sources; a proof type | the command |
| Racing writers | fetch.md: temp name and rename, its DST "not built" | the DST with a planted early rename | nothing |
| No vouching record | a standing `Fetch` record with its file deleted: graph.rs fails "the next command asks again", but `Fetch` does not rerun (from reading, untested) | presence by key | the command |
| Records read in place | names.rs `Reader::read` reads each record whole into a frame; `.ok()?` turns an IO error into absence | map; typed IO error | the command |
| Many members | names.rs `world`: `PathPackages` when more than one path package is reached (blaze's own workspace) | not this part's; blocks blaze's checkout | the command |
| Offline error | `CANNOT_FETCH` (13) without `fetch`; with it, the machine's `Why` | every place tried (`local::Tried`) | the command |
| Records from the shared cache | `--origin <folder> --origin-key` (main.rs), `v2/shared` public records; keys hold `Tool` = the binary's build id (home.rs `tool`) | a release identity in public keys; HTTP origin (remote-read.md "Next" 1); a bundle a lock (blaze-global bundle.rs) | the command (folder origin only) |
| Deferred sources | every reached package is fetched before the graph goes on | fetch only on a record miss | nothing |
| Memory floor | whole answer bodies in the heap (85 to 117 MiB peak; floor about 12 MiB) | body to a scratch file | the command |
| Git from another tool | none | cargo's git db read by the pinned commit's tree | nothing |
| Harness stores | tempo_front's `<fetch-store>` argument: a store filled by `examples/fetch.rs` by hand | the command's store is the same records, so it goes | only an example |

### Pieces, in build order

1. **Adopt a local archive** (blaze-fetch `src/local.rs`, written): `crates_io_folder`,
   `archive_name`, `adopt`/`adopt_in` (SHA-256 then `crate_record_in`), `Try`, `Miss`, `Tried`.
   Test: another valid archive under the lock's name is refused; the adopted record equals the
   network path's. Depends on nothing. With piece 2 it is the shortest route for blaze's own
   checkout: all 297 registry archives of its lock are in cargo's cache on this Mac (counted).
2. **The command adopts** (blaze-cli fetch.rs, driver lane): `FetchEnv` gains the cargo home
   (`CARGO_HOME`, else `<home>/.cargo`, read in main.rs); where `held_record` is false (no
   `fetch`) and in `net::Net::lookup` (with it), list `registry/cache` through `Fs::home`, keep
   `crates_io_folder`, sorted, read `<folder>/<archive_name>`, `adopt`, write the record as `put`
   does, answer held. Test: a temp cargo home with one `.crate`, a lock pinning it, no network:
   the graph holds the package and no finding 13; a tampered archive gives the finding with tries.
3. **Sources in the home** (fetch.rs `record_path`, graph.rs `run`'s folder, names.rs `Folder`;
   store.rs `Place` of a `Fetch` record to `Own`): `<home>/.blaze/v2/sources`. Test (worktree
   tests): a second worktree fetches and adopts nothing. Same lane as 2, after it.
4. **Racing writers, proven** (the publish of piece 3): unique temporary name, whole bytes, rename
   onto the key; a simulation of two commands, a crash mid-write; oracle: no reader maps a record
   that fails its digest, none is lost; planted bug: rename before the bytes are whole. Parallel.
5. **Verified once a boot**: a `boot` file beside the sources (the machine store's form), a proof
   type that `held_record`'s BLAKE3 makes; test: a second command in one boot hashes 0 bytes;
   planted: the proof made without the check. After 3.
6. **Presence by key**: the graph task's failed map reruns that unit's `Fetch` instead of failing
   the command. Test: delete a source file between two commands; the second answers. After 3.
7. **The machine's own local step** (blaze-fetch `fetch.rs`, `drive.rs`, `FetchIo::local`): the
   machine asks for a crate's local archive before HTTP, so 2's glue in `lookup` goes. Test: the
   simulation with a cargo cache, some archives tampered; oracle: none adopted, a held one asks no
   HTTP; planted: adoption before the checksum. After 2.
8. **The offline finding**: finding 14 `source-not-on-machine` (blaze-report words.rs) with
   `Tried` and the network's outcome. After 2.
9. **Names maps records** (names.rs `Reader`, `Folder`): `Mapping` as graph.rs does; IO errors
   typed. Parallel.
10. **Bodies to scratch files** (fetch.md open item 2): peak from 93 to about 12 MiB. Parallel.
11. **Multiplexed client** for static.crates.io (blaze-http's h3, or HTTP/2), proven on the
    simulated origin only. Parallel; after the bandwidth floor is met.
12. **Public keys by release identity**, then a bundle a lock and the HTTP origin (store lane):
    a CI record is this binary's record. Needs a decision. Parallel.
13. **Deferred sources**: `Fetch` wanted only on a record miss. After 6 and 12.
14. **Git from cargo's bare repositories**, bound to the pinned commit's tree. After 7.
