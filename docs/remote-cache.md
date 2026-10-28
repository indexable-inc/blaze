# A shared remote cache: one store for CI and everyone

2026-10-27. Status: slice 1's gateway is built and deployed (`tools/remote-cache`, API-v0.md,
measured in `tools/bench/remote-cache/gateway-v0.tsv`), and so is slice 3's server side: `/has`, `/get`,
`/blob`, `/put`, the D1 location table and the private HMAC namespaces (`tools/remote-cache/API.md`,
`tools/bench/remote-cache/cas-v1.tsv`; `/put` decompresses with workerd's own zstd, `node:zlib`, not
`ruzstd`). Slice 4's server side is deployed too: key lists, heads and segments with every
index-write check, the refresh inside head writes, the 80-day location expiry, and the lifecycle
rules (API.md "Index", `tools/bench/remote-cache/index-v1.tsv`). One deviation: the refresh rewrites each
old object onto its own name through the R2 binding (a streamed get and put, R2 checking the MD5 or
SHA-256), not S3 `CopyObject`, since no R2 S3 credential is readable to the Worker; measured live, a
rewrite moves the upload date. The client speaks v0: `BLAZE_REMOTE=blaze+https://<gateway>/t/<tenant>/v3`
with `BLAZE_REMOTE_TOKEN` pushes and pulls today's packs through the gateway
([#525](https://github.com/indexable-inc/blaze/pull/525), `tools/bench/remote-cache/e2e-client.tsv`), and
`.github/actions/blaze-remote` is CI's step for it (pull on every run, push from main after the
tests), which goes into the merge gate once the gate builds with blaze and a release contains the
client. The client's use of `/has`, `/get`, `/put` and the index needs #283. On blaze's own store
(`tools/bench/remote-cache/selfbuild.tsv`, ix arm64 VM in us-west-2), a pull of 121 packs, 237.8 MB, took
8.6 s (26.5 MiB/s), against 51 to 55 s for cargo's no-edit build with main's target dir. The build
from a warm store is not measured yet: a cold `blaze build -p blaze-engine` of blaze needs more than
16 GB (OOM-killed at 14.8 GB on 4 CPUs; 28.3 GB peak where it finished, in 389.6 s), so main's
store could not be made in the VMs that could be placed that day, and the gate's 16 GB runners hit
the same limit on a cold store. The Worker is TypeScript rather than workers-rs: its tests run on real R2 and KV bindings
in workerd, and a Rust part would need rustc to build its wasm while blaze cannot build `wasm32`
(blaze-builds-blaze). The sans-IO `blaze-gateway-core` below stays the plan for the native server
and for the Worker once blaze builds wasm. Written against blaze main `e4bbb88` and
`blaze-next` `5c5299f` ([#283](https://github.com/indexable-inc/blaze/pull/283)). Each claim is
marked **read** (in a source we read, or measured by us), **reported** (a document or someone
says so, not checked again here) or **unverified** (our inference or an estimate). "Slice R2"
below is [ci-runner.md](ci-runner.md)'s second slice; "R2" alone is Cloudflare's object store.

Andrew wants CI and local testing to be "uber fast", and wants other people to benefit from the
cache. [AGENTS.md](../AGENTS.md) makes blaze's own store "the one cache", locally and in the
shared remote CI fills, and rules out sccache. This file is the design of that shared remote:
how CI on main fills it, how CI on every branch and any user reads it, and how nobody else can
put code in it. It builds on [cache.md](cache.md) (the store and today's remote), on
[ci-runner.md](ci-runner.md) (test results keyed by content) and on [cargo.md](cargo.md) (the
sandbox and `BLAZE_TESTS=affected`).

## Short version

- **Two layers.** A content-addressed store (CAS) of immutable blobs, each named by the SHA-256
  of its uncompressed bytes, kept in packs of 16 to 64 MB. Above it an index that maps an input
  key (a function's key, a dependency unit's key, a test's key) to the blob that is its output.
  The CAS cannot lie: every blob is hashed after it is fetched. The index can, so every entry is
  under an Ed25519 signature that clients check, and only CI on main holds the key.
- **A gateway in front of R2.** No client ever touches the bucket. A Cloudflare Worker on the
  cache's domain holds the only R2 binding (no egress charge) and serves a small API: `/has`,
  `/get` (one streamed response of many blobs), ranged single gets, `/put` (it hashes every blob
  again before storing it, so the CAS cannot be poisoned), and per-tenant index reads and
  writes. It does auth, rate limits, metering and the edge cache, and the storage behind it can
  be swapped. Index entries belong to a tenant (`blaze` is the public one CI on main fills).
- **Public and private blobs.** Public blobs (CI of open projects, crates.io dependencies) are
  shared, and anyone may get one by its hash. A private tenant's blobs dedupe only within that
  tenant and are stored under `HMAC(tenant key, hash)`, so even the bucket's object names say
  nothing about their content. A tenant reads public blobs and its own, never another
  tenant's. "Not found" and "not yours" are the same 404 from the same code path, and a private
  upload is always uploaded in full. This closes the dedup side channels of
  [Harnik et al. 2010](#threat-model).
- **The key stops naming the blaze binary.** Today an entry is keyed by what blaze was built from
  (or, for the published crate, the bytes of its executable), so every release, several a day,
  misses everything. The dev-aws-14 store shows it: seven near-identical packs of 203 to 229 MB
  written in ten hours, the same code stored once per blaze build (**read**). The new key names a
  codegen version that changes only when output would, and a CI check on a fixed corpus keeps it
  honest.
- **Fetch time.** blaze's own workspace is about 270 to 320 MB in the store (**read**, below). At
  the measured 210 to 225 MiB/s from R2 to a GitHub runner over 16 streams that is about 1.3 s,
  against about 8 s on one stream (**unverified** arithmetic on **reported** rates, measured to
  R2 directly; the rate through the Worker is measured in slice 1). The fetch starts when the
  daemon starts and overlaps the workspace load.
- **Sans-IO, on blaze-core.** Hashing, chunking, packing, the index, signatures, the gateway's
  API and both sides of it are pure state machines in their own crates. The client does no IO
  of its own: it pushes `blaze-core` effects and is given completions, as the VFS's journal does
  on `blaze-next` ([#283](https://github.com/indexable-inc/blaze/pull/283)). `blaze-core` gains
  socket effects, so the same client runs under the seeded simulation, the threads driver and
  the io_uring driver, with TLS and HTTP/1.1 in sans-IO code above them. The gateway's logic is
  one crate run by the Worker (workers-rs) and by a native server for tests and self-hosting.
- **One hash, SHA-256**, for the CAS, because the Worker checks every upload with the runtime's
  native SHA-256 and every neighbour (the store, Cargo.lock, OCI, Nix) already names bytes by it.
  #283's VFS hashes with BLAKE3; the doc proposes it move to SHA-256 while nothing persists its
  hashes, so a VFS file node's name is its CAS name ([Hash](#hash-sha-256)).
- **Chunking** with VectorCDC (the SIMD form of AE, a hashless content-defined chunker) for large
  opaque files: rlibs, dependency units, the toolchain, and any blob over 256 KiB. Compiled
  functions below that are not chunked; their key already deduplicates them. **Compression** is
  zstd per blob, a high level on CI's upload, with trained dictionaries for the small function
  blobs.
- **GC is R2's lifecycle rules, no job**: heads expire 28 days after upload, segments 32, blobs
  90. When a head is written, the gateway copies onto itself every segment the head names that is
  more than two days old and every pack holding a named blob that is more than 45 days old, so a
  blob always outlives every entry that names it.
- **Rollout** in nine slices; the first two (the gateway over R2 serving today's remote, then
  the codegen version) give most of the speed.

## Goals and non-goals

Goals:

- CI on any branch, and a user's first build of a project, take every function, dependency unit
  and test pass that main's CI already made, and compile or run only what differs.
- Anyone can read the shared tier with no account and no token, from anywhere, at no cost to them
  and about a hundredth of a cent per cold fetch to us (see [cost](#cost-and-latency)).
- Nobody but CI on main can make a client run code the shared tier serves, whatever they do to
  the bucket, the gateway or the network. A user's own tenant is trusted by that user's keys
  only.
- No client holds a storage credential. The gateway is the one door: it authenticates, limits,
  meters and checks every write, and the storage behind it can change without any client
  changing.
- No request tells a tenant whether some content exists, unless the content is public or the
  tenant stored it itself.
- A new blaze release that does not change what blaze compiles keeps every entry.
- Every part that decides what to fetch, how to check it and how to write it can be tested
  without a network, under `blaze-core`'s seeded simulation, and the driver that does the IO
  can be replaced.

Non-goals:

- Remote execution. A function compiles in milliseconds and a test runs in a sandbox locally in
  about 8 ms (**read**, [ci-runner.md](ci-runner.md#protocol-why-none-on-the-hot-path)); there is
  nothing to execute remotely. REAPI stays an edge adapter
  ([#226](https://github.com/indexable-inc/blaze/issues/226)).
- Writes to the shared tier from anyone but CI on main: no pull request, no fork, no user.
- A rust-analyzer analysis cache. The analysis is rebuilt at load, as today ([cache.md](cache.md)).
- Keeping every main commit forever. What main has not used for a month or three expires.
- Sharing between different codegen versions or toolchains. A different version is a different
  key, by design.
- Encrypting a private tenant's blobs against the gateway. The gateway and Cloudflare see what
  is stored; encryption on the client would end sharing blobs with the public tier.

## What exists today

From [cache.md](cache.md) and `crates/blaze-cargo/src/remote.rs` (**read**):

- The remote is a directory, `s3://` (the AWS SDK) or a read-only `https://` address. It holds
  `packs/<sha256>.pack` (or `.pack.zst`), `keys/<sha256>` (the keys in one pack),
  `manifests/<project>` (one line per pack) and `units/`, `deps/` for cargo's dependency builds.
- A pack is named by the SHA-256 of its bytes and checked after the fetch. The manifest is the one
  mutable object; it is signed with Ed25519 when `BLAZE_REMOTE_SIGN` is set, carries a turn
  number that only goes up, and a machine refuses a turn lower than one it took (`signed.rs`).
- A daemon with `BLAZE_REMOTE` pulls before its first build (all packs, or with
  `BLAZE_REMOTE_LAZY` only the key lists and then a pack when a key in it is first asked for),
  and pushes every five minutes and when it ends (`keeper.rs`).
- A fetch that fails its hash is thrown away, reported, and the functions are compiled again.
- Real S3 was never run: everything was measured against MinIO and SeaweedFS over loopback.
- No workflow in `.github/workflows` sets `BLAZE_REMOTE` (**read**). CI fills no remote today.

The identity every store key is hashed with is `Identity(FORMAT, build, toolchain)`
(`crates/blaze-store/src/lib.rs`, **read**). `build` is a hash of every source file of the
workspace (`crates/blaze-engine/build.rs`), or the executable's bytes for a blaze built elsewhere. So
any commit that touches a crate of blaze, a doc-comment fix in the CLI included, gives a new
identity and an empty cache.

## Prior art

| System | What it does | What we take, or not |
|---|---|---|
| Bazel REAPI ([remote_execution.proto](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto)) | CAS of `Digest{hash, size}` plus an ActionCache from action digest to result. AC entries carry no signature; trust is the server's access control. Recently `GetChunkMapping`/`SpliceBlob` with FastCDC 2020 (avg 512 KiB recommended) | The CAS/AC split. Not its unsigned AC (our gateway is not trusted for integrity, so the index must carry its own proof), and not its per-entry unary lookups: 165,752 functions would be 165,752 calls (**read**, [ci-runner.md](ci-runner.md)) |
| [Attic](https://github.com/zhaofengli/attic) | Nix cache on S3: FastCDC 16/64/256 KiB for NARs of 64 KiB and up, zstd, narinfo signed by the server, GC by retention then orphans | Same chunk sizes as a start; the index expiring before the blobs it names. Not server-side signing: our gateway checks signatures but holds no signing key, so a compromised gateway cannot forge an entry |
| casync / [desync](https://github.com/folbricht/desync) | Chunk stores `<4 hex>/<id>.cacnk`, chunks 16:64:256 KB, zstd, SHA-512/256 or SHA-256, index files listing chunks | A chunk list per large file; over S3, HTTP and OCI alike |
| Nix binary cache ([narinfo](https://nix.dev/manual/nix/2.35/protocols/binary-cache/narinfo.html)) | Ed25519 over a fingerprint, `trusted-public-keys = name:key`, keys named `cache.example.org-1` and incremented to revoke; rotation signs with both keys ([nix#12976](https://github.com/NixOS/nix/pull/12976)) | Named, numbered keys and an overlap when rotating |
| sccache 0.19 ([S3.md](https://github.com/mozilla/sccache/blob/main/docs/S3.md)) | S3 and R2 (`SCCACHE_ENDPOINT`, `SCCACHE_REGION=auto`), `SCCACHE_S3_NO_CREDENTIALS` for public read-only PR builds. Keys by BLAKE3 over the sysroot's shared libraries, arguments, sources and env ([Caching.md](https://github.com/mozilla/sccache/blob/main/docs/Caching.md)) | Read-only anonymous access for PRs. Not the tool (AGENTS.md forbids it), and not keying by the compiler's bytes, which is the problem this design removes. Whether it hashes the rustc binary itself we did not confirm (**reported**) |
| [kache](https://github.com/kunobi-ninja/kache) | Content-addressed Rust cache over S3, GCS or OCI | Nothing new beyond the above |
| VectorCDC ([FAST '25](https://www.usenix.org/conference/fast25/presentation/udayashankar)) | SIMD forms of the hashless chunkers RAM and AE: VRAM 24 to 26 GB/s with AVX-512, VAE 5.2 GB/s with 256-bit vectors, against FastCDC 2 GB/s and Gear 0.95 GB/s on the paper's machine (**reported**) | The chunker, via ix's implementation (below) |

VectorCDC is not a gear hash: it vectorises AE and RAM, which find a local byte maximum and need
no rolling hash at all, and the paper reports that its SIMD approach did not speed up FastCDC
(**reported**). The decision stands; only the description changes.

## Key and identity

### Function keys

The key of a function is what [cache.md](cache.md) describes (backend, optimisation level,
target, debug info, overflow checks, the stable name with generic arguments and crate), and the
identity it is hashed with becomes:

    Identity = SHA-256(STORE_FORMAT, CODEGEN, toolchain commit)

with no `build` field. `CODEGEN` is a number in a new crate, `crates/blaze-semver` (its only
content), and there is one such number per kind of entry:

| constant | covers | bump when |
|---|---|---|
| `CODEGEN` | compiled functions: MIR lowering, the backends, layout, mangling, relocations, unwind and line tables, the record's encoding | the bytes of any stored function for some key would change |
| `EXPAND` | proc-macro expansions | an expansion's stored form or its key changes |
| `SCRIPT` | build-script runs | the stored run or its key changes |
| `UNIT` | dependency units | a unit's contents for one key would change |
| `TESTS` | test pass records | the reach hash, the sandbox's recording, the fixed environment or the harness changes what a pass means |

A toolchain bump (`harness/toolchain`) is still a new identity: the std and the language blaze
matches move with it ([AGENTS.md](../AGENTS.md), own-nightly). The local store takes the same
identity, so what the remote serves drops into it unchanged.

### Keeping the version honest

A version that is not bumped when output changes serves the old blaze's output to the new blaze:
a codegen fix that never takes effect on cached functions, or, worse, functions of two layouts
linked into one program. So the version is checked, not trusted.

- **The golden check.** A gate job (`harness/semver.py`, in the merge gate) builds a fixed corpus
  and hashes every stored value per kind into one digest per key, written to
  `harness/semver/<target>.txt` (key, value digest). The corpus is blaze's own workspace, the five
  bench apps, and every regression test of the codegen and difftest harnesses. If any digest
  differs from the committed file and the constant of that kind was not bumped in the same change,
  the gate fails and names the first keys that differ. A bump regenerates the file.
- **Determinism.** The job builds the corpus twice, in two fresh stores, and fails when the two
  differ. Nondeterministic output is a bug
  ([#200](https://github.com/indexable-inc/blaze/issues/200) for whole-program layout); without
  determinism no cache is sound.
- **Bug fixes force a bump.** A codegen bug fix comes with its reproducer in the corpus
  ([AGENTS.md](../AGENTS.md), best-long-term-design). The fix changes that reproducer's output, so
  the golden check fails until `CODEGEN` is bumped. A fix that changes nothing in the corpus is a
  fix without a test.
- **Coverage is the hole.** A change that alters output only for code outside the corpus passes.
  The corpus is large on purpose, and slice 2 measures how many keys it has against a typical
  project.
- **Dev builds.** A blaze built from a dirty tree, or not by CI, reads the shared tier and writes
  only the local store and private tenants. The shared tier is written only by CI on main, after
  the golden check passed on that commit.

### Other keys

- **Dependency units.** `SHA-256(UNIT, toolchain, target, package id, version, the
  Cargo.lock checksum or git commit, features, profile settings, the unit keys of its
  dependencies)`. The output is a unit record: each file's path, mode, size and chunk list.
- **Test passes.** As [ci-runner.md](ci-runner.md#what-a-test-result-is-keyed-by) says, in two
  steps, because what a test read is only known after it ran. The entry key is
  `SHA-256(TESTS, identity, target, reach hash, attributes, fixed environment, the content of each
  CARGO_BIN_EXE_* it runs)`. Its value is a list of pass records, each holding the paths the
  sandbox recorded with their digests (absent paths included) and the seed it ran with. A client
  checks those paths on its own machine; when all match, the test is skipped as passed. Only
  passes are kept: a failure is never a reason to skip.
- **Flakes.** Two verdicts under one key are nondeterminism by construction
  ([ci-runner.md](ci-runner.md#nondeterminism-and-flakes)). CI on main writes a `flake` entry under
  the same key when a rerun disagrees, so every client knows the test is main's bug and not the
  pull request's. The seed is a hash of the key, so a rerun sees the same randomness.
- **Proc-macro expansions and build-script runs**, as cache.md keys them, under `EXPAND` and
  `SCRIPT`.
- **Build scripts and proc macros, built and run.** Two steps too, as tests are: a first key
  of what the build or run is declared to be made of (its rust-project description and the
  toolchain; for a run, the build of its script, its arguments and the environment it was
  given, without the checkout's paths), whose slots hold the sets of files it was seen
  reading, and a second key of those files' digests, which holds the executable or library,
  or what the run printed and left in `OUT_DIR` ([cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox)).
  They are ordinary store entries, so push, pull and the gateway carry them unchanged. A run
  outside the sandbox is never kept, so never pushed.

## The gateway

Clients never get the bucket. A Cloudflare Worker, `blaze-gateway`, answers on the cache's
domain (`cache.<domain>` below) and is the only holder of an R2 binding to the bucket. Reads
through a binding cost no egress, and no client, CI job included, holds an R2 credential. The
Worker authenticates, applies the access rule, hashes every uploaded blob again, rate-limits,
meters, and keeps hot objects in the edge cache. Everything it decides is in a sans-IO crate
behind a `Store` trait, so R2 can be swapped for S3 or a disk without a client noticing.

### Tenants, namespaces and tokens

A **tenant** owns index entries: heads and segments. A tenant is either **public** (`blaze`,
filled by CI on main; later other open projects) or **private** (a user or a company). Blobs live
in **namespaces**: one public namespace, shared by every public tenant, and one private namespace
per private tenant.

- A **public blob** is named in storage by its SHA-256. Anyone may get it by that hash, with no
  token. Only public tenants' write tokens may put one.
- A **private blob** is named in storage by `HMAC-SHA256(k_t, sha256)`, where
  `k_t = HKDF-SHA256(ikm: gateway secret v, salt: the tenant's random salt, info: "blaze tenant
  key" ‖ tenant id ‖ v)` is the tenant's store key. The salt and the key version `v` are kept with
  the tenant in D1, and tenant ids are never reused, so a new tenant never inherits an old
  namespace and the gateway secret can be rotated tenant by tenant (each rotation rewrites that
  tenant's names, a copy of its objects). A private blob dedupes only within its tenant: two
  tenants that upload the same bytes store them twice. The object names, the location table and
  the private packs' own indexes (which the gateway writes with HMAC names, not hashes) do not
  reveal a private blob's hash. What this protects is **names**: someone who can only list the
  bucket, or read D1, cannot test a guess. The frames themselves are plaintext zstd, so whoever
  can read objects (a leaked R2 credential, an operator) can decompress them and hash what they
  find. Encrypting private packs at rest under a key derived from `k_t` would close that, and is
  an [open question](#open-questions). The gateway sees content either way
  ([non-goals](#goals-and-non-goals)).
- A private tenant reads public blobs and its own private ones; its index may name both. It never
  reads another tenant's private blobs, and nothing it can do tells it whether another tenant
  stored something.

A **token** is a 256-bit random bearer secret. Workers KV maps `SHA-256(token)` to
`{tenant, scope: read | write, expiry}`; revoking deletes the entry, and since KV is eventually
consistent that reaches every location in about 60 s (**reported**,
[KV](https://developers.cloudflare.com/kv/concepts/how-kv-works/)). A request with no token is
anonymous: it reads public blobs and public tenants' index. CI on main holds a `blaze` write
token, `BLAZE_CACHE_TOKEN`, in the `cache-writer` environment.

### The API

Bodies are deterministic CBOR, as everywhere in blaze ([AGENTS.md](../AGENTS.md), no-mcp).

| request | does |
|---|---|
| `GET /t/<tenant>/trust` | the tenant's signed key list |
| `POST /t/<tenant>/heads {names}` | the heads among `names` that exist, signed, with their ETags; one request for 50 ancestors or 500 unit keys |
| `GET /t/<tenant>/seg/<sha256>` | one index segment |
| `POST /has {hashes}` | for each hash: present in the caller's private namespace, present in the public namespace, or absent |
| `POST /get {hashes}` | one streamed response of the blobs, as a pack stream: per blob its hash, dictionary, lengths and zstd frame, in the order the Worker reads them |
| `GET /blob/<sha256>` with `Range` | one blob's stored frame, or a range of it: for the lazy mode and to resume |
| `POST /put` (a pack stream, at most 32 MB) | stores the blobs into the caller's namespace; answers with each blob's place |
| `PUT /t/<tenant>/seg` | writes one segment, after checking every blob it names is readable by the tenant. The gateway names it by the SHA-256 it computes and writes it only if absent, so no token can overwrite a segment |
| `PUT /t/<tenant>/head/<name>` | writes one signed head, after checking its signature, tenant and turn, and refreshing what it names ([GC](#gc)). Every head write is conditional: the client sends `If-Match` with the ETag it read (or `If-None-Match: *` for a new head), and the gateway's own put carries the same condition in `onlyIf`, so the turn check and the write are one atomic step; a 412 means re-read, merge, re-sign and retry |
| `PUT /t/<tenant>/trust` | the key list, signed by the tenant's root key |

The Worker finds a blob through a location table in D1:
`blobs(ns, key, object, offset, length, ulength, dict)`, with `key` the hash for the public
namespace and the HMAC for a private one, and `objects(object, uploaded)` for refresh. A `/get`
or `/has` of thousands of hashes is one query per about 25,000 hashes (a JSON array bound as one
parameter: D1 allows 100 parameters and 2 MB per value, **reported**,
[limits](https://developers.cloudflare.com/d1/platform/limits/)).

### The access rule, and how it is applied

The namespaces a request may look in are fixed from its token **before** any lookup: the public
namespace, plus the caller's own private namespace when the token names a private tenant. The
gateway then derives each key (`sha256` for public, `HMAC(k_t, sha256)` for private) and looks
only there. So:

- **One 404.** "Absent" and "stored by another tenant" give the same answer from the same code
  path: there is no lookup in another tenant's namespace to take a different time or return a
  different code. The same holds for index reads: a private tenant's head or segment fetched with
  another tenant's token is the 404 a missing one is.
- **No upload shortcut.** `/put` into a private namespace takes and stores the whole body, every
  time, as one new pack. There is one location row per `(ns, key)`, and it always points at the
  newest copy, which lives longest; an older copy is left to expire. The answer and its timing do
  not depend on what other tenants have. A client skips uploading what
  `/has` or its own index says its tenant already has, which reveals only its own data to it.
- **Public reads reveal public data.** That a public blob exists is not a secret: it was built
  from open source by CI. Whether it still exists depends only on public tenants' heads
  ([GC](#gc)): a private tenant's use never keeps a public object alive, so public `/has` and
  `/get` say nothing about private tenants.
- **Rate limits** per tenant on `/has` and `/get` (and per IP for anonymous requests) cap how many
  guesses anyone can test, even against their own or public data. They use the Workers Rate
  Limiting binding, which counts per Cloudflare location and is approximate (**reported**,
  [rate limiting](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/)).

### Checks on write

- **Every blob is hashed by the gateway.** `/put` decompresses each frame (pure-Rust `ruzstd`,
  which runs in the Worker's WebAssembly; dictionaries come from the caller's readable
  namespaces) and refuses the whole request when one does not hash to the name it came with, or
  when its uncompressed length exceeds what its header says or 4 MiB. Then the frames are written
  as one `.bpk` into the caller's namespace, named by the SHA-256 the gateway computes over it
and put only if absent, and recorded in D1. So the CAS cannot be poisoned,
  even by a stolen write token.
- **Every index write is checked**: a segment names only blobs the tenant can read (one D1 query);
  a head's signature verifies against the tenant's current key list, its signed `tenant` field is
  the token's tenant, its turn is above the stored one (checked against the ETag the write is
  conditional on, so two writers cannot both pass), and every segment it names exists in that
  tenant's index. The gateway checks signatures to keep junk out; clients check them again,
  because the gateway is trusted for confidentiality and availability, never for integrity
  ([Trust and signing](#trust-and-signing)).
- **No delete** in the API. Objects leave only by the lifecycle rules.

### Abuse, metering and billing

Every request writes a data point to Workers Analytics Engine: tenant, operation, bytes in and
out, storage reads and writes, D1 rows, CPU time. That is the meter future billing reads; it costs
nothing per request at this volume (**unverified**). Denial of wallet is bounded by the rate
limits, a per-invocation CPU limit in the Worker's configuration, the 32 MB `/put` body, and the
4 MiB cap on a frame's uncompressed size.

### Cost and latency

Cloudflare's prices (**reported**,
[Workers](https://developers.cloudflare.com/workers/platform/pricing/),
[R2](https://developers.cloudflare.com/r2/pricing/),
[D1](https://developers.cloudflare.com/d1/platform/pricing/)): Workers $0.30 per million requests
and $0.02 per million CPU milliseconds beyond the $5 plan's allowance, calls from the Worker to its
bindings not billed as requests; R2 $0.36 per million reads, $4.50 per million writes, $0.015 per
GB-month, no egress; D1 $0.001 per million rows read and $1 per million rows written. The $5 Workers
Paid plan includes 10 million requests and 30 million CPU milliseconds a month, and D1 25 billion
rows read and 50 million rows written a month; R2 includes 10 GB-month, a million writes and ten
million reads (**reported**). D1 counts a row written for each index a write touches as well, and
`blobs` needs one index beside its key (on `object`, for the refresh), so a new blob is about 2 rows
written. The table gives the price beyond the allowances; below them the marginal cost is nothing.
Per request (**unverified** arithmetic):

| request | what it costs us |
|---|---|
| a head, segment or trust list from the edge cache | about $0.32 per million |
| the same on an edge miss | plus an R2 read: about $0.68 per million |
| a `/get` of 20 MB, cold at that location | 1 request, about 20 R2 reads of 1 MiB windows, one D1 query, tens of CPU ms: about $0.000008 |
| the same, hot | about $0.000001 |
| a cold CI fetch of blaze's workspace (300 MB) | about 40 requests and 300 R2 reads: about $0.00013 |
| a `/put` of 32 MB | about a second of CPU (decompressing in WebAssembly, hashing natively), one R2 write, and about 2 D1 rows per blob, about 160,000 rows: about $0.16 |
| a main commit's push (10 to 30 MB of new blobs) | about $0.05 to $0.15, nearly all D1 rows |
| a cold fill after a `CODEGEN` bump (about 800,000 blobs) | about $1.60 |

The D1 rows are the cost that matters, and it is per new blob, not per read. The included 50 million
rows a month cover about 25 million new blobs, about 30 cold fills or several hundred main pushes,
so at blaze's own volume it costs nothing beyond the plan. If it grows past that, one row per run of
consecutive blobs in a pack, with the run's hashes in the pack's own index, cuts it by the run
length; slice 3 measures it.

Latency, from a runner or a developer near a Cloudflare location (**unverified**, slice 1
measures all of it): a Worker isolate starts in a few milliseconds; a head or segment from the
edge cache is about 10 to 30 ms to the first byte; an edge miss adds the trip to R2's region,
about 30 to 150 ms; a D1 query adds the trip to its primary, the same order. A `/get` streams as
its R2 reads arrive: a Worker keeps at most 6 subrequests waiting for headers
(**reported**, [limits](https://developers.cloudflare.com/workers/platform/limits/)), so the
client gets parallelism from 16 `/get` requests in flight, not from one. The rate of one
streamed Worker response is not known to us and is the first number slice 1 takes.

## Threat model

The gateway is a shared store, so what one tenant uploads must not become an oracle for
another. Harnik, Pinkas and Shulman-Peleg showed that cross-user deduplication in cloud storage
lets anyone test whether a given file exists (confirmation of a file) and, for files with a few
unknown bits, find those bits by uploading every candidate and watching whether the service
already has it (learn the remaining information) ("Side channels in cloud services:
deduplication in cloud storage", IEEE Security & Privacy 8(6), 2010,
[doi:10.1109/MSP.2010.187](https://doi.org/10.1109/MSP.2010.187), **reported**). The signals were
whether an upload was skipped, its bytes on the wire, and its time. Build outputs are
deterministic, which makes guessing easier: the attacker can build each candidate exactly.

Examples, and what stops each:

- **A build script bakes a low-entropy secret into an output.** A company's `build.rs` writes a
  4-digit region code or a license key with 20 bits of freedom into a generated `.rs` file whose
  function is then compiled. An attacker builds the same crate for each of the 10,000 candidates
  and asks whether the cache has the result. Stopped: the company's blobs are in its private
  namespace under HMAC names; the attacker's `/has` and `/put` look only in the public namespace
  and the attacker's own, so every candidate is absent, by the same path.
- **A test fixture holds a Social Security number.** The fixture file is a blob, and a test pass
  record names its digest. Nine digits are 10^9 guesses, far fewer once the area and group are
  known. Stopped the same way; on top, the per-tenant rate limit on `/has` makes even probing
  one's own data slow.
- **Confirming that a company uses a library at a given commit.** A dependency unit's key is
  computable from a `Cargo.lock`, so a unit head or unit blob for `acme-internal-fork` at commit
  `abc123` would prove that the company builds it. Stopped: index reads are per tenant, a
  private tenant's heads 404 to everyone else exactly like absent ones, and its units' blobs are
  private. crates.io units built by the public tier are public by design: they say what blaze's
  CI built, not who else uses it.

What remains, stated rather than solved:

- **Edge-cache timing of public blobs.** The edge cache is per location. A fast answer for a
  public blob tells the asker that someone near that location fetched it lately. It reveals use
  of open-source code near a city, not which tenant. Accepted; an open question if a tenant
  needs more.
- **Which units a machine asks for**, seen by Cloudflare and us (open questions).
- **Members of one tenant.** A company tenant shared by many people is one namespace: its
  members can probe each other's uploads through `/has`, rate-limited. A team that needs more
  uses a tenant per person or project.
- **The gateway sees private content.** A compromised gateway or Cloudflare can read and withhold
  private data, never make a client run code (signatures and hashes are checked by the client).

## Object layout in the bucket

The bucket exists: `blaze-cache`, in Indexable's Cloudflare account. The repository has the
secrets `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY` and `R2_ENDPOINT`, from an object read and
write token scoped to that bucket alone, checked with `aws s3 ls` (**reported**, by the
coordinator, 2026-10-27). With the gateway they leave GitHub: the Worker reads and writes through
its binding, the [refresh](#gc)'s rewrites included. Its lifecycle rules are in [GC](#gc).
Only the gateway reads or writes these names; everything except heads and trust lists is immutable.

    trust/<tid>/keys                          the tenant's signing keys, signed by its root key; never expires
    index/heads/<tid>/p/<project>/<commit>    a signed head: one main commit of one project
    index/heads/<tid>/p/<project>/latest      a signed head: the newest of the above
    index/heads/<tid>/u/<unit-key>            a signed head: what any project reuses of one dependency unit
    index/seg/<tid>/<sha256>                  an index segment (sorted entries, deterministic CBOR)
    cas/pub/<sha256>.bpk                      a pack of public blobs, named by the SHA-256 of its bytes
    cas/t/<tid>/<hmac>.bpk                    a pack of one private tenant's blobs, named by HMAC(k_t, its SHA-256)

`<tenant id>` is an opaque number in every path, public tenants included, and is never reused;
the API's `/t/<tenant>` name maps to it in D1. For a private tenant the gateway also replaces
every name after the id (project, commit, unit key, segment hash) with its HMAC under `k_t`, so a
listing of the bucket shows that some tenant has some heads and packs, and nothing of what they
are or whose.

The kind comes before the tenant in every name because lifecycle rules match literal prefixes,
and the three prefixes are the three lifetimes of [GC](#gc): `index/heads/` about 28 days from
upload, `index/seg/` about 32, `cas/` about 90. The location table and the token map are in D1
and KV, not in the bucket.

A `<project>` is a name CI picks (`blaze`, `ripgrep`, the bench apps). A `<commit>` is a main
commit's id. A `<unit-key>` is the hex of a dependency unit's key.

### Blobs and packs

A blob is named by the SHA-256 of its **uncompressed** bytes. That name survives a change of
compression level or dictionary, and it lets a client check one blob read by a range out of the
middle of a pack, without the rest. There are three kinds: a function record (what the store
holds today under one key), a chunk of a large file, and a small record (a unit record, a test
pass list, an expansion).

A pack (`.bpk`) is a header, then frames, then an index, then a fixed footer that gives the index's
offset and length so one ranged read of the tail finds it. Each frame is one zstd frame holding one
blob. The index lists, per frame: the blob's SHA-256, its offset and compressed length in the pack,
its uncompressed length, and the dictionary it was compressed with (by SHA-256, or none). A pack's
own name is the SHA-256 of all its bytes. Packs are 16 to 64 MB: big enough that a project's whole
store is tens of objects, small enough that one is a few ranged reads and fits Cloudflare's 512 MB
cacheable limit with room ([default cache
behavior](https://developers.cloudflare.com/cache/concepts/default-cache-behavior/), **reported**).

This is also the store's next format (`STORE_FORMAT` 4): the local store keeps the same packs and
reads a value by decompressing its frame. A fetched pack is put in the store as it is and mapped,
not compressed again. Today's packs compress 128 KB blocks together, which a small value with a
dictionary can match on its own (to be measured in slice 6).

CI writes the functions of one crate together, in the order the daemon walks them, so a client
that needs most of a crate reads one contiguous range.

### Index segments and heads

A segment is a sorted table of entries, `(kind, key) → value`, encoded as deterministic CBOR (the
same encoding the agent API uses, [AGENTS.md](../AGENTS.md), no-mcp). A value names blobs by
SHA-256 and uncompressed length only; where a blob is stored is the gateway's business (its
location table), so storage can be repacked or moved without an index change. A segment is
named by the SHA-256 of its bytes and holds at most about 4 MB, so a project is tens of
segments.

A head says what one main commit of one project used:

    head {
      tenant: "blaze", name: "p/blaze/9f3c...", turn: 1793500800, created: 2026-10-27T03:40:00Z,
      versions: { store: 4, codegen: 41, expand: 7, script: 3, unit: 2, tests: 5 },
      toolchain: "nightly-2026-10-25", target: "x86_64-unknown-linux-gnu",
      segments: [ sha256, ... ],
      signer: "blaze-ci-1", signature: ed25519(over everything above)
    }

The signed `tenant` field means a head cannot be replayed into another tenant's index: the
gateway refuses it on write and a client refuses it on read. A project head names every segment
that main's build at that commit read or wrote: it is the prefetch list. A unit head
(`index/heads/<tid>/u/<unit-key>`) names the segments holding what any build has
of that dependency unit: its rlib chunks, its functions, its non-generic code. A user's project
that depends on `serde 1.0.228` with the same features finds those by the unit key, computed from
its own `Cargo.lock` before any request. That is how people other than us benefit: every
dependency blaze's CI has built, for its own workspace or the corpus, is reusable by anyone with
the same version, features, target and toolchain.

Generic instances are kept under the crate whose types they are instantiated with: an instance of
`Vec<serde_json::Value>` belongs to `serde_json`'s unit head when all its type arguments come from
that unit's dependency closure, and to the user's crate otherwise (the user's own crates are not
in the shared tier).

### Large files

A file over 64 KiB that is opaque to blaze (an rlib, a static library, an archive in a unit, the
toolchain's tarball) is cut into chunks by VectorCDC, minimum 16 KiB, average 64 KiB, maximum
256 KiB, and stored as a chunk list: the chunk SHA-256s in order, with the file's own SHA-256.
The same holds for **any** blob over 256 KiB, whatever its kind: a function record with a large
constant table or a big test pass list is chunked too, so no frame comes near the gateway's
4 MiB cap and no build output can fail a `/put` by its size. Two
builds of a crate that differ in a few functions share most of their chunks. The sizes are those
of Attic, desync and ix's own chunker (**reported**), and are tuned in slice 5 against blaze's
units: dedup ratio across consecutive main commits against index size. A file under 64 KiB is one
blob.

The cut points are part of the format: the scalar implementation is the definition, every SIMD
path (AVX-512, AVX2, NEON) must find the same cuts, and a property test checks that on random and
on real inputs. Changing the parameters changes chunk lists, not blob names; dedup across the
change is lost, nothing else.

## Hash: SHA-256

Two hashes are in play. The store and today's remote name everything by SHA-256
([cache.md](cache.md)). The VFS of [#283](https://github.com/indexable-inc/blaze/pull/283)
(`crates/blaze-vfs` on `blaze-next` `5c5299f`) names a file node by `blake3::hash` of its bytes and
a directory node by BLAKE3 keyed with `derive_key("blaze vfs directory v1")` (**read**,
`blaze-vfs/src/lib.rs`). The CAS needs one name per blob. **It is SHA-256**, for two reasons, the
second of which decides it:

1. **The Worker checks every upload, and SHA-256 is native there.** A blob's name is the hash of its
   uncompressed bytes, so the Worker decompresses each frame (`ruzstd`, in WebAssembly, whatever the
   hash) and then hashes the result. With SHA-256 that hash is a call into the runtime's native
   WebCrypto, `crypto.subtle.digest`, or its streaming form `crypto.DigestStream("SHA-256")`, which
   workers-rs wraps as `worker::crypto::DigestStream` (**read**,
   [web-crypto](https://developers.cloudflare.com/workers/runtime-apis/web-crypto/#constructors),
   `workers-rs/worker/src/crypto.rs`). With BLAKE3 it would be the `blake3` crate compiled to
   WebAssembly (portable code, or the `wasm32_simd` feature where the runtime has SIMD, **read**,
   BLAKE3's `Cargo.toml`), at a speed nobody has measured in a Worker (**unverified**), on top of
   the decompression, billed by the CPU-millisecond and inside the CPU limit of a 32 MB `/put`. The
   R2 binding's `put` also takes a `sha256` and rejects a body that does not match it (**read**, [R2
   Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)), which
   checks the pack's own name, not the blobs'. This reason is real but not large: it saves part of
   the Worker's CPU and keeps the check in Cloudflare's code. The second decides it.
2. **Identity across systems.** SHA-256 is the name everything blaze touches already uses for the
   same bytes: the store and today's remote, Cargo.lock's `checksum` of every crates.io `.crate` (so
   a public dependency blob can be asked for by the checksum in the lock file, with nothing hashed
   first), OCI, Nix and REAPI's default digest. The only BLAKE3 name is the VFS's, and it has not
   shipped: the journal does write hashes (a directory record names its children by hash, a head
   record its root), but a node record carries what the node is made of, not its own hash, and
   loading interns every node again (**read**, `blaze-vfs/src/journal.rs` on `blaze-next`). So
   changing it now is a journal format change before any journal exists outside a test; changing the
   CAS later would rename every blob.

**Recommendation for #283: hash VFS nodes with SHA-256 too**, files as SHA-256 of their bytes (so a
file node's hash is its CAS name, and a root shipped to another machine is fetched by the hashes it
already holds) and directories as SHA-256 over a domain prefix (`"blaze vfs directory v1\0"`) and
the same entry encoding (a new journal magic). The cost measured so far is a tie on one thread:
BLAKE3 1.9 to 2.1 GiB/s (2.0 to 2.3 GB/s) on one M5 Max core in the roadmap (**read**,
`docs/roadmap.md` on `blaze-next`), SHA-256 2.22 GB/s with SHA-NI on dev-aws-14 and 2.38 GB/s with
the ARMv8 extensions on an M2 Max (table below). What BLAKE3 gives that SHA-256 does not is its tree
mode: one large file hashed on many threads, and a range verified alone (Bao). The CAS needs
neither: no blob is larger than 256 KiB once [Large files](#large-files) has cut it, and each is
verified whole. If #283 keeps BLAKE3, the bridge is one alias per file node in the local store
(BLAKE3 node to SHA-256 blob, written when the node's bytes first enter the store, one extra hash of
bytes already in cache) and a VFS root shipped remotely becomes a blob listing both names per file.
That is the fallback, not the proposal; it is an [open question](#open-questions) for
#283's authors.

| machine | SHA-256, one core | source |
|---|---|---|
| AMD EPYC 9R45 (Zen 5), SHA-NI, OpenSSL 3.5.9 | 2.22 GB/s (BLAKE2b-512: 1.64 GB/s) | **read**, dev-aws-14, 1 MiB blocks |
| Apple M2 Max, Rust `sha2` | 2.38 GB/s | **reported**, [checksum-benchmarks](https://github.com/conradludgate/checksum-benchmarks) |
| Apple M4 Pro, CommonCrypto | 3.28 GiB/s | **reported**, [notonbluray](https://notonbluray.com/blog/benchmarking-aes-256-sha256-and-crc-64-on-apple-silicon/) |

Against the network: 16 streams from R2 deliver about 225 MB/s of compressed bytes, which
decompress to about five times that, 1.1 GB/s (compiled code compresses to about a fifth,
[cache.md](cache.md)). Hashing 1.1 GB/s takes half a core with SHA-NI. A GitHub runner has four.
BLAKE3 with AVX-512 and many threads would save part of that half core (**reported**), not
enough to pay for the two points above. The Rust crate is `sha2` 0.11, which picks SHA-NI or the
ARMv8 extensions at run time (**reported**,
[README](https://github.com/RustCrypto/hashes/blob/master/sha2/README.md)).

## Compression

- **zstd, one frame per blob.** CI compresses what it uploads at a high level: a blob is written
  once and read by every CI run and user after, and zstd decompresses at about the same speed
  whatever the level. Level 19 runs at a few MB/s a core (**unverified**, to be measured), which
  is fine for a main commit's new blobs (tens of MB) and slow for a cold fill after a `CODEGEN`
  bump (about 1.5 GB uncompressed, about a minute on 8 cores at 3 MB/s a core, **unverified**).
  So the level is a setting measured in slice 6: high for incremental pushes, lower for a cold
  fill that a background job compresses again later. The blob names do not change.
- **Dictionaries for small blobs.** A function record is about 400 bytes on average (nushell:
  165,752 functions in 64 MB after zstd, **read**, [ci-runner.md](ci-runner.md)), too small for
  zstd to find much in on its own. CI trains a dictionary (`zstd::dict::from_samples`, 112 KiB)
  per (codegen version, target, blob kind) from a sample of main's blobs, and compresses each
  small blob with it.
- **How dictionaries are named and versioned.** A dictionary is a blob like any other, in the same
  namespace as the blobs compressed with it, named by the SHA-256 of its bytes, and named in each
  pack's index and in every segment whose entries use it, so it is fetched, checked and refreshed
  with them. The gateway needs it to check an upload, so it is put before the blobs that use it. The
  zstd frame also carries a 32-bit dictionary id, which is a hint only: we set it from the hash's
  first bytes, mapped into the range zstd leaves for private use (over 32,767 and under 2^31,
  [format](https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md)). A new
  dictionary is a new hash; packs and segments written with the old one keep naming it. Retraining
  happens on a `CODEGEN` bump and when slice 6's measurement says the ratio has drifted.
- **Large chunks** are compressed without a dictionary, and kept as they are when compression
  saves less than an eighth.

## Trust and signing

The CAS needs no trust: a blob that does not hash to its name is refused. The index is where a
writer could poison clients: an entry that maps a real key to other code. So every entry is under
a signature, and the signature is checked before anything it names is used.

### What is signed

One Ed25519 signature per head, over the head's fields, which name segments by SHA-256, which hold
the entries. So each entry is covered by exactly one signature check and one hash. Signing each
entry on its own would cost 64 bytes an entry and a verification each: about 165,000 verifications
for nushell, several seconds of one core at tens of microseconds each (**unverified**), against one
per head. `ed25519-dalek` does the checks (2.2 in the workspace today).

### The keys

- **Root key** `blaze-root-1`: offline, held by Andrew. Its public half is compiled into blaze
  and registered with the gateway as the `blaze` tenant's root. It signs only the key list.
- **The key list** (`GET /t/blaze/trust`): a list of signing keys, each with a name (`blaze-ci-1`),
  its public key, its role (signs project and unit heads of this tenant), not-before and not-after
  times, and a turn number. Signed by the root key. Clients fetch it at daemon start (it is small
  and cached for a minute at the edge) and remember its highest turn in the local store. A CI runner
  starts with no memory, so each blaze release also compiles in a **minimum key-list turn** (the
  newest list's at release time), and a client accepts a head only if its signing key is in the
  list, `now` is before the key's not-after, and the head's `created` is inside the key's window. CI
  keys get a short not-after, about 60 days, and CI switches to the next key when fewer than 29 days
  are left, so a key never signs a head that outlives it (heads live 28 days). A private tenant has
  the same structure with its own root key, which it gives the gateway when it registers and its
  clients in their configuration.
- **CI signing key** `blaze-ci-N`: an Ed25519 secret in a GitHub environment, `cache-writer`,
  that only workflows on `refs/heads/main` can enter. The gateway write token
  `BLAZE_CACHE_TOKEN` sits in the same environment; no R2 credential is in GitHub at all. Today
  `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY` and `R2_ENDPOINT` are repository secrets, which every
  workflow run on a branch of this repository can read (not a fork's); slice 1 moves them into
  the Worker's secrets and deletes them from the repository before any workflow uses the cache.
  The job that signs and uploads is a separate job from the build: it receives the
  build's new blobs and entries as an artifact, so build scripts, proc macros and tests (which run
  sandboxed in the build job, [cargo.md](cargo.md#sandboxed-tests-an-experiment-opt-in)) never run in a
  process that can read the key.

### Rotation and revocation

- **Rotation**, every few months or at will: add `blaze-ci-(N+1)` to the key list (root-signed, turn
  + 1), switch CI to it, and keep `blaze-ci-N` until its not-after date, so heads it signed stay
  valid while they are still live. This is the overlap Nix uses
  ([nix#12976](https://github.com/NixOS/nix/pull/12976)).
- **Revocation**: publish the key list without the key (turn + 1). A client refuses any head
  signed by a key the current list does not have, so revocation reaches each client at its next
  daemon start and the heads the key signed stop being read. A fresh client that is served an
  older list (by a compromised gateway or cache) still refuses it below the release's minimum
  turn, and in any case stops trusting the revoked key at its not-after, at most about 60 days
  later; the next release raises the minimum. CI then signs fresh heads for the
  live main commits with the new key.
- **Root compromise**: a blaze release with a new root public key. Nothing in the bucket can
  recover from it, which is why the root key is offline and signs only the key list.

### Who can write

| who | reads | writes |
|---|---|---|
| CI on main, `cache-writer` job | public | the `blaze` tenant and public blobs, with `BLAZE_CACHE_TOKEN` and the signing key |
| CI on a branch of the repository | public, anonymously | nothing (no environment, no token) |
| CI on a pull request from a fork | public, anonymously | nothing: GitHub gives fork runs no secrets |
| any user | public, anonymously; their own tenant with its token | their own tenant, with its token and their keys |
| the gateway | everything | locations, refresh copies; never a head, since it holds no signing key |

### What it guarantees, and what not

- Tampering with the bucket, the gateway, the edge cache or the network: caught. Blobs fail their
  hash; entries fail their signature or are absent. The gateway is trusted to keep private data
  private and to serve what it has, not to tell the truth: a client checks every hash and
  signature itself.
- A stolen gateway write token without the signing key: can upload blobs that hash correctly
  (storage, metered and rate-limited) and segments no signed head names. Cannot delete, since the
  API has no delete, and cannot write a head. Cannot make a client run anything. Revoked by
  deleting its KV entry.
- A stolen R2 credential (the Worker's copy secret, read and write): can delete or overwrite
  objects, which clients see as 404s and hash failures. Can read private packs, which are
  plaintext zstd (a listing alone shows only HMAC names and opaque tenant ids). Cannot make a
  client run anything.
- A replayed older head or key list: refused by its turn, as `signed.rs` does for manifests now,
  against the highest turn the machine remembers and, for key lists, the minimum compiled into the
  release. A stale `latest` from the edge cache is fetched again past the cache, never run as
  wrong code.
- A compromised main build: not caught by signatures. CI signs what main's build produced. If a
  dependency's build script escaped the sandbox and changed an output before it was hashed, the
  signed entry is wrong. Two mitigations, neither complete: the signer is not reachable from the
  build (above), and a second CI job on another runner compiles a random sample of the new entries
  of each push and compares them with what is signed (this needs per-function determinism, which
  the golden check enforces). A mismatch fails main and revokes nothing automatically.
- Users' assumption, as today ([cache.md](cache.md#trust-and-signing)): two builds of blaze with
  the same versions compile the same key to equivalent code.

### Private tiers are tenants

`BLAZE_REMOTE` becomes a list of tenants, read in order; the shared one is the default last
entry:

    BLAZE_REMOTE="https://cache.<domain>/t/acme?token=env:ACME_CACHE_TOKEN&trust=acme-root-1:<pubkey>,shared"

Each entry has its own trust: `shared` is the `blaze` tenant with the root key compiled into blaze;
a private tenant names its root key in the address (`trust=`) or in `BLAZE_REMOTE_TRUST`, and its
token by the environment variable that holds it, never inline. The client sends the token as
`Authorization: Bearer`, never in a URL, so it stays out of logs and caches. The address can be
another gateway (`blaze-gateway-server`, self-hosted) as well as ours. A machine writes only to a
tenant it holds a write token and a signing key for. A private tenant's segments may name public
blobs, which it then does not upload; when the public copy ages, the gateway copies the blob into
the tenant's namespace rather than keeping the public object alive ([GC](#gc)). `off` turns every
entry off.

## The fetch path

### What happens when the daemon starts

1. **Before the workspace loads**: fetch the tenant's key list and the project head. The project
   is the checkout's `BLAZE_PROJECT`, else a name derived from its git remote; the head is that
   of the merge base of `HEAD` with main, else the newest of the first 50 first-parent ancestors,
   else `latest`: one `POST /t/<tenant>/heads` with all 52 names. Two requests, each a round trip
   to the nearest Cloudflare location (tens of milliseconds, **unverified**).
2. **Segments**, all at once: tens of `GET /t/<tenant>/seg/...` of up to 4 MB, from the edge.
3. **Units**: from the checkout's `Cargo.lock`, compute every dependency unit key and ask for the
   unit heads the project head does not already cover, in one `POST /t/blaze/heads`, so a project
   with no head of its own (any user's) still gets its dependencies.
4. **Plan**: the union of blobs the segments name, minus what the local store has, in the
   segments' order (which is crate and walk order), cut into batches of about 4 to 20 MB of
   expected compressed size.
5. **Fetch**: 16 `POST /get` in flight (`BLAZE_REMOTE_JOBS`, default 16). Each response is
   decompressed frame by frame as it streams, each blob hashed against its name, and written
   into the local store.
6. **Meanwhile** rust-analyzer loads the workspace (52 s cold for nushell, **read**,
   [ci-runner.md](ci-runner.md)). Store lookups during the load and the first build
   find what has arrived; a key whose blob is still in flight waits on that part rather than
   compiling it (the store's existing hook, `fetch_missing_with`, used by `BLAZE_REMOTE_LAZY`
   today).

The lazy mode stays: with `BLAZE_REMOTE_LAZY` step 5 fetches only what a build asks for. With a
project head the full prefetch is the default, since the head names what main's build actually
used.

### How much, and how long

blaze's own workspace in the store, measured on dev-aws-14 on 2026-10-27 (**read**): the
machine's shared store held 2.35 GB in 55 packs, written by several lanes over ten hours under
several blaze identities. The packs one self-host build wrote between 03:34 and 03:39 come to about
317 MB; one earlier group (00:22 to 00:23) to about 273 MB. Of those, one pack of 203 to 229 MB
appears seven times, each time under a new identity, which is today's per-release miss made
visible. So one identity's working set for blaze is about 270 to 320 MB at zstd level 3. With a
higher level and dictionaries it may be 15 to 25% smaller (**unverified**, slice 6 measures).

Rates from R2 to a GitHub `ubuntu` runner: about 38 MiB/s on one stream, 210 to 225 MiB/s with 16 in
parallel (**reported**,
[cloud-cache-action](https://xsavikx.github.io/cloud-cache-action/guide/performance.html)). Those
were measured against R2 directly, not through a Worker. A `/get` adds a Worker between the two, on
the same edge; whether one streamed Worker response reaches 38 MiB/s, and whether 16 reach the
aggregate, is what slice 1 measures first. The table assumes they do.

| what | size | 1 stream (38 MiB/s) | 16 streams (215 MiB/s) |
|---|---|---|---|
| blaze's workspace, whole, level 3 | 300 MB | 7.5 s | 1.3 s |
| the same at a higher level, 20% smaller | 240 MB | 6.0 s | 1.1 s |
| a branch that touched one crate: what main's head lacks | 10 to 30 MB | 0.3 to 0.8 s | 0.05 to 0.15 s plus a round trip |
| nushell's function store ([ci-runner.md](ci-runner.md)) | 64 MB | 1.6 s | 0.3 s |

All **unverified** arithmetic on the figures above. The CPU on the client: 300 MB compressed is
about 1.5 GB of blobs, which hash in about 0.7 core-seconds and decompress in about 1 to 1.5
core-seconds (**unverified**), spread over the runner's cores while the bytes arrive. On a warm
developer machine almost all of it is in the local store already and the fetch is the heads and
segments, a few hundred milliseconds.

After this the fetch is not the long pole. helix's warm start spent 129 to 131 s of thread time
turning store records back into the daemon's values
([research/warm-start.md](research/warm-start.md), **read**); that cost is the store's, and #216 is
its fix.

### HTTP versions

- The client speaks only to the gateway's domain, a Cloudflare zone: HTTP/1.1, HTTP/2 and, with
  the zone's switch on, HTTP/3 between the client and Cloudflare
  ([HTTP/3](https://developers.cloudflare.com/speed/optimization/protocol/http3/), **reported**).
  Worker routes are served on all of them.
- The gateway reaches R2 through its binding, not over HTTP the client sees. So R2's S3
  endpoint, which answered no HTTP/3 and had HTTP/2 disabled in 2023 (**reported**,
  [answeroverflow](https://www.answeroverflow.com/m/971611340068245504),
  [community](https://community.cloudflare.com/t/cloudflare-r2-http-2-error/441326)), matters only
  for the gateway's server-side copies.
- The 16-stream figure was measured with parallel connections. Sixteen streams on one HTTP/2
  connection share one TCP congestion window and may not reach it; the client opens
  several connections and slice 1 measures how many.

### Edge caching

The Worker caches with the Cache API, which is per Cloudflare location. Segments and trust lists are
cached for a week and 60 s; heads for 60 s, and a head write purges its entry at the location that
took the write (other locations may serve a stale head for up to a minute; a client that has seen a
newer turn fetches it again past the cache, [Failure modes](#failure-modes)). Packs are cached in 1
MiB-aligned windows keyed by the object's storage name, so one window serves every `/get` that
touches it; objects never change while they exist, and a refresh copies the same bytes. Cached
windows are internal: the cache is consulted only after the access rule has said which namespaces
the request may read, so it adds no path by which another tenant's data could answer. The cache's
one signal, a faster answer for a public blob someone near the same location fetched lately, is in
the [threat model](#threat-model). Cloudflare caches objects up to 512 MB; our packs are at most 64
MB.

## The write path

Only the `cache-writer` job on main writes the `blaze` tenant. After the build and test job of a
main commit passes the golden check and finishes:

1. **Collect** what the build wrote to its store that the tenant lacks: the build job lists the
   keys it wrote, and the latest head's segments say what is there already. `/has` answers for
   any hash the segments do not settle.
2. **Pack**: compress each new blob at the upload level, with the dictionary for its kind, into
   pack streams of up to 32 MB, ordered by crate and walk order. Large files are chunked first.
   New dictionaries go first.
3. **Put**: `POST /put`, 4 to 8 at a time. The gateway hashes each blob, writes a `.bpk` and its
   D1 rows, and answers. A retried `/put` stores a second copy of what it already stored, harmless
   and expired like the rest.
4. **Segments**: `PUT /t/blaze/seg`, entries for the new blobs, and for the project head a full
   list of the segments its build used (old segments reused by hash). The gateway refuses a
   segment that names a blob the tenant cannot read.
5. **Sign and write heads**: `PUT /t/blaze/head/p/<project>/<commit>` (`If-None-Match: *`), every
   touched unit head, then `latest`, each with `If-Match` on the ETag read before. On a 412 the job
   re-reads the head, merges its own segments into the newer one (unit heads are written by many
   projects), signs again with a turn above the newer one, and retries, a bounded number of times,
   as `extend_list` does today. Before it stores a head, the gateway refreshes what the head names
   ([GC](#gc)); the request returns when the head is stored.
6. **Test passes and flakes** of the commit's test run go into a segment of the same head.

Readers never see a head naming an object not yet written, because objects go up before the
segments and heads that name them, and the gateway refreshes anything old a head names before it
stores the head. A job killed midway leaves unnamed objects, which expire with their prefix. A
push of a main commit that fails fails main's train run: a broken writer is seen the same day.
What a push costs is in [Cost and latency](#cost-and-latency).

## Failure modes

The rule ([AGENTS.md](../AGENTS.md), no-fallback): no quiet other path, and a clear error where
blaze cannot do what it was asked. A remote is a cache: with or without it, a build produces the
same output, so not reaching it changes speed, not what is built. A remote that answers wrongly
is a different matter: that is a bug or an attack, and must be seen and fixed.

| failure | what blaze does |
|---|---|
| a tier cannot be reached (DNS, connect, timeout, a 5xx from the gateway after retries) | locally: the build goes on without that tier, and **every** build's `Finished` line says so (`[blaze, remote down: cache.<domain>: timed out]`), not one line in a log. With `BLAZE_REMOTE_REQUIRED=1`, which blaze's own CI sets, it is an error and the job fails |
| a blob that does not hash to its name | error, naming the object, the tier and the head; after one retry that asks the gateway to bypass its edge cache. Today the build compiles those functions instead; that changes to an error |
| a head or key list with a bad signature, an unknown, revoked or expired key | error, naming the head and the key |
| a head with an older turn than this machine has seen | the edge cache may hold a head for 60 s at a location the write did not purge, so: fetch it again asking the gateway to bypass the edge cache. If that answer is still older, error (a rollback); if it is not, go on with it. A key list older than the release's minimum turn or the remembered one is an error at once |
| a head names an object the tier does not have (404) | error: by the invariant of [GC](#gc) a blob outlives every entry that names it, so this is a bug |
| 401 or 403: a token unknown, expired, revoked or without the scope | error naming the tenant, the operation and the token's variable; never a quiet fall back to anonymous |
| 429 from a rate limit | wait the `Retry-After` the gateway gives, a bounded number of times; then as an unreachable tier (the status line, or an error in CI) |
| the gateway refuses a write: a blob that does not hash to its name, a segment naming a blob the tenant cannot read, a bad signature, a wrong tenant, an older turn | error in the write job, with the gateway's reason; main's train run fails |
| no head for this project, or none for this codegen version | not an error: the tier has nothing for this build yet. The `Finished` line says `remote: nothing for codegen 41` |
| the local disk fills while fetching | as today: `store: nothing more is kept of this build`, and the build goes on |
| the write job fails | main's train run fails, with the step and the request |

Each error says how to go on: `BLAZE_REMOTE=off` for this command, or the tier to drop from the
list.

Why not stop on an unreachable tier by default too: a developer on a train with the shared tier
configured would have every build fail for a cache. The status on every build makes it
impossible to miss. CI, where a cache that silently stopped working is a regression nobody sees,
treats it as an error. This is the one choice here Andrew may want the other way (open questions).

## GC

There is no GC job. R2's lifecycle rules expire an object by its age since upload, not since its
last read, so the design makes upload age mean "last used by a head":

| prefix | expires after | what keeps it |
|---|---|---|
| `index/heads/` | about 28 days | CI on main writes a new head for each commit and rewrites `latest` and the unit heads it touches |
| `index/seg/` | about 32 days | the gateway copies onto itself every segment a new head names that is more than 2 days old |
| `cas/` (public and private packs) | about 90 days | the gateway copies onto itself every pack holding a blob a new head's segments name that is more than about 45 days old |
| `trust/` | never | written by hand with a root key |

**The refresh** runs inside `PUT .../head/...`, before the head is stored. The gateway reads the
segments the head names **from R2, not the edge cache** (a cached segment can outlive its
object), takes each segment's age from the same read's upload date and each pack's from D1's
`objects(object, uploaded)` (packs only), and copies each one that is too old onto itself with the
S3 API's `CopyObject` and `x-amz-metadata-directive: REPLACE` (S3 refuses a copy onto itself
without it), which moves no bytes through the Worker (the R2 binding has no copy, so this one call
uses the S3 API with a Worker secret, signed by `rusty-s3`). Whether R2's copy onto itself resets
the upload date that lifecycle rules count from is **unverified** and is slice 4's first test, so
the gateway does not trust it: after each copy it `HEAD`s the object, writes the upload date R2
reports into `objects.uploaded`, and fails the head write if the date did not move. Only public
heads refresh public objects; a private head's old public blobs are copied into its own
namespace instead (below). The copies are tens per main commit; the request takes as long as they
do.

**The invariant: a blob always outlives every index entry that points to it.** A head stored at
time `t` expires no earlier than `t + 28 days`. Every segment it names was uploaded or copied at
`t - 2 days` or later, so it expires no earlier than `t + 30 days`, after the head. Every pack
holding a blob one of those segments names was uploaded or copied at `t - 45 days` or later, so
it expires no earlier than `t + 45 days`, after the segment. A later head that names the same
segment refreshes it and its packs again by the same rule, so the chain holds for every head
that is still live. The margins absorb the lifecycle's day granularity (a rule deletes at or
after its age, never before) and a write job that runs for hours. Readers check nothing of it: a
404 for a named object is a bug and an error ([Failure modes](#failure-modes)).

The same rule keeps private tenants' data, with one difference that matters for the
[threat model](#threat-model): **a private head never refreshes a public object.** If it did, a
public pack the `blaze` tenant stopped naming would stay as long as some private head named it,
and anyone could learn, from whether an old crates.io unit is still there, that some private
tenant uses it. So when a private head names a public blob whose pack is past the 45-day mark,
the gateway copies that blob (its frame, read by range through the Worker) into a new pack in the
tenant's own namespace and adds the tenant's location row for it; the tenant's `/get` looks in
its own namespace before the public one, so it reads its copy from then on. Public liveness, and
so public `/has` and `/get`, depend only on public heads. The copy is metered to the tenant, and
its timing depends only on the public pack's age, which public heads alone decide.

What follows from it:

- A head nobody writes again (a project not built on main for a month, an old commit) disappears
  after 28 days, and then a branch based on that commit misses. The merge-base lookup goes back 50
  commits, so this matters only for branches about a month old.
- A pack no live head uses expires 90 days after its last refresh. A pack with one used blob and
  many dead ones is refreshed whole; the write job puts the used blobs again into a new pack
  when under half of a pack's blobs are used, and the location table moves to the new one.
- Unit heads are rewritten when CI touches the unit; one untouched for 28 days expires, and a
  user's next build of that dependency misses until CI builds it again.
- Released blaze versions' std projects are heads like any other: kept while the release train
  writes them, expired 28 days after the last write.
- D1 rows of expired objects: a row whose object's `uploaded` is more than about 80 days old is
  treated as absent, and the gateway deletes such rows in the background of the writes that
  touch the table, a bounded number per request. No separate job.
- Cost of the refreshes: a copy per old pack and per reused segment per main commit, tens of
  writes at $4.50 a million.

The rules on `blaze-cache`, added 2026-10-27 with `wrangler r2 bucket lifecycle add` after the
refresh was deployed and checked live: `expire-index-heads` (`index/heads/`, 28 days),
`expire-index-seg` (`index/seg/`, 32 days), `expire-cas` (`cas/`, 90 days), beside
`abort-multipart` (incomplete multipart uploads, 1 day) and Cloudflare's default (7 days). No rule
touches `trust/` or v0's `v3/`. A self-hosted
`blaze-gateway-server` over S3 uses S3 lifecycle rules with the same prefixes; over a disk it
expires by the same ages itself.

## Crate layout

Sans-IO throughout, on the core that [#283](https://github.com/indexable-inc/blaze/pull/283)
adds. On `blaze-next`, `crates/blaze-core` defines the contract: a state machine pushes
`Effect`s, each with a `Tag` it chose, and is handed a `Completion` with the same tag; a
`Driver` (`submit`, `poll`, `in_flight`, `now`) runs them. Three drivers exist: `sim::Sim`, one
thread under a seeded scheduler that delays, reorders and fails completions and whose `crash()`
keeps what was synced plus a random prefix of the rest; `threads::Threads`, blocking calls on a
few threads (the macOS default); and `uring::Uring`, one io_uring set up with the raw system
calls, no thread of its own (**read**, `blaze-core/src/{lib,sim,threads,uring}.rs`). The VFS's
journal is the first state machine on it. The cache is the next: it adds no IO layer and no
async runtime of its own.

### What blaze-core gains

Today's effects are files and timers only: `ReadAll`, `Read`, `Append`, `Sync`, `Truncate`,
`Timer` (**read**). The client needs the network, so `blaze-core` gains socket effects, each
implemented by all three drivers:

| effect | completes with | `Threads` | `Uring` | `Sim` |
|---|---|---|---|---|
| `Resolve { host }` | `Resolved(Vec<SocketAddr>)` | `getaddrinfo` on a thread of its own | the same, on a helper thread whose completion `poll` collects, so a slow DNS server never stalls the ring | a table |
| `Connect { addr }` | `Connected(Conn)` | `TcpStream::connect` on the connection's thread | `socket` then `IORING_OP_CONNECT` | a link to an in-process peer |
| `Send { conn, bytes }` | `Sent` (all of it) or `Failed` | `write_all` on the connection's thread | `IORING_OP_SEND` | delivered after a seeded delay, or the link is cut |
| `Recv { conn, max }` | `Read(bytes)`, empty when the peer closed | `read` on the connection's thread | `IORING_OP_RECV` | any prefix of what is there, so frames split anywhere |
| `Close { conn }` | `Closed`; every effect still pending on `conn` completes `Failed` first | shut down the socket, ending the thread | `IORING_OP_ASYNC_CANCEL` of the pending ones, then `IORING_OP_CLOSE` | |
| `Cancel { tag }` | the cancelled effect completes `Failed("cancelled")`, or normally if it won the race | for a socket effect, shut down the connection | `IORING_OP_ASYNC_CANCEL` | removed from the pending set |
| `Listen { addr }`, `Accept { listener }` | `Listening(Conn)`, `Accepted(Conn)` | as above | `IORING_OP_ACCEPT` | the peer connects |

`Conn` is a small id the driver hands out, as a `FileName` names a file. `Threads` gives each
open connection a thread of its own rather than queueing socket calls on its shared pool: a
blocking `Recv` must never sit in front of an `Append` or a `Sync`, and 16 parked receives on a
pool of a few threads would stall everything. A timeout is a `Timer` and a `Cancel` of what it
guards, so a state machine never leaves an effect pending (which would also keep
`run_until_idle` from returning). `Sim`'s far end is a `Peer` trait object in the same process;
the tests plug in an adapter that runs `blaze-gateway-core` with its `Store` over `Sim`'s own
files and its locations in memory, so one seed drives client, network, gateway and disk, and a
failing run (a reset in the middle of a pack, a `/put` whose response is lost, a crash after a
pack is appended and before its sync) replays exactly. The alternative, a `Driver` generic over
its effect type so sockets live outside `blaze-core`, is an [open question](#open-questions) for
#283's authors; the proposal is the variants above, since the ring and its reaping loop are
already there.

TLS and HTTP are not effects. They are sans-IO code between the client's state machines and the
socket effects: `rustls`'s unbuffered client (`UnbufferedClientConnection`, which takes and
returns byte buffers and does no IO, **reported**) and an HTTP/1.1 codec over `httparse`. Several
HTTP/1.1 connections give the parallel streams [HTTP versions](#http-versions) asks for. HTTP/2
is left out: the `h2` crate is written against tokio's IO traits (**reported**), and the measured
16-stream rate came from parallel connections anyway. HTTP/3 would be `quiche`'s own sans-IO
connection (datagrams in, datagrams out, **reported**) over two more effects, `SendTo` and
`RecvFrom`, not tokio-quiche.

Writing what was fetched is the store's existing kind of effect: a pack is `Append`ed and
`Sync`ed, and the local index names it only after its `Sync` completed, so a crash leaves at
worst an unnamed tail that the next start cuts, as the VFS journal's recovery does.

### The crates

Every crate below the client state machines takes bytes and returns bytes, with no sockets,
files, clocks or `blaze-core` dependency. The client state machines speak `blaze-core` effects.
The gateway's logic is the same kind of crate, with effects of its own (R2, D1, the Cache API,
none of which a Linux or Mac driver has), so the client's tests run against the real gateway
under `Sim`.

| crate | what it is | depends on |
|---|---|---|
| `blaze-core` (exists, #283) | effects, drivers, the simulation; gains the socket effects above | `libc` |
| `blaze-vfs` (exists, #283) | the Merkle VFS; with the [hash recommendation](#hash-sha-256), its `Hash` is `blaze-cas`'s `Digest` | `blaze-core`, `blaze-cas` |
| `blaze-semver` | the version constants, nothing else | none |
| `blaze-cas` | `Digest` (SHA-256), streaming verification (`Verifier::push(&[u8])`, `finish() -> Result`), chunk lists | `sha2` |
| `blaze-cdc` | VectorCDC AE: scalar reference, AVX-512, AVX2, NEON; a streaming chunker that is fed bytes and yields cut points | none. Ported from ix's `crates/storage/cdc` (below) |
| `blaze-pack` | the `.bpk` format and the pack stream: writer and reader over byte slices, the frame index, dictionary references | `blaze-cas`, `ruzstd` to read, `zstd` (feature) to write |
| `blaze-index` | entry kinds, segments (deterministic CBOR), heads with their tenant, key lists, Ed25519 signing and checking, turns | `blaze-cas`, `ed25519-dalek`, a CBOR crate |
| `blaze-gateway-api` | the wire API: request and response types, their CBOR, status codes. The one contract between client and gateway | `blaze-index`, `blaze-pack` |
| `blaze-http` | sans-IO HTTP/1.1 client and server codec, and TLS through `rustls`'s unbuffered API: connection bytes in and out, requests and responses with streamed bodies | `httparse`, `rustls` |
| `blaze-remote-proto` | the client as state machines: `Fetch` (trust, heads, segments, plan, `/get` batches, packs appended and synced into the store) and `Push` (collect, `/has`, pack, `/put`, segments, heads). Each pushes `blaze-core` effects and consumes completions | `blaze-core`, `blaze-http`, `blaze-gateway-api`, `blaze-pack`, `blaze-index` |
| `blaze-gateway-core` | the gateway as state machines: token to tenant and namespaces, the access rule, `/put` checking, index-write checks, refresh planning, cache keys, meter events. Pushes its own tagged effects (`Store`: `get` with range, `head`, `put` with conditions, `copy_onto_itself`; `Locations` queries; `Meter` events) and consumes their completions, in `blaze-core`'s tag-and-completion shape | `blaze-gateway-api`, `blaze-index`, `blaze-pack`, `hmac`, `hkdf`; `blaze-core` for its tests only (`sim::Rng`, the `Peer` adapter) |
| `blaze-gateway` | the Worker: the driver of `blaze-gateway-core`'s effects, over the R2 binding, D1, KV, the Cache API, the Rate Limiting binding and Analytics Engine, and the S3 `CopyObject` signed by `rusty-s3`. Built for `wasm32-unknown-unknown` with workers-rs | `blaze-gateway-core`, `worker` |
| `blaze-gateway-server` | the same core natively, on a `blaze-core` driver: its `Store` is files through `Append`, `Sync` and `Read`, its locations an append-only table, its listener `Listen` and `Accept` effects (`IORING_OP_ACCEPT` on the ring) and `blaze-http`'s server side. For CI's end-to-end check and self-hosting behind a TLS terminator | `blaze-gateway-core`, `blaze-core`, `blaze-http` |
| `blaze` (`remote.rs`, `keeper.rs`) | runs a `Fetch` at daemon start and a `Push` when told, on the daemon's driver (until the daemon runs one, a `Threads` driver of its own); owns policy (tenants, env vars, status lines) | `blaze-remote-proto`, `blaze-core` |
| `blaze-store` | reads and writes format 4 packs through `blaze-pack` | `blaze-pack` |

Once #283 lands the order is: `blaze-core` gets the socket effects in its own change (with
`Sim`'s `Peer` and tests in the shape of its existing ones), `blaze-vfs` switches its hash to
`blaze-cas::Digest` if the recommendation is taken, and the cache crates land on top. Nothing in
the cache depends on `blaze-vfs`; the VFS meets the cache only through the store, where a file
node's bytes are a CAS blob named by the node's hash, so a root published as an index entry
(kind `vfs-root`, a later slice) is fetched by the hashes the tree already holds.

Drivers, and what each is for:

- **`Threads`**: the default on macOS and the first one used on Linux. Blocking sockets, one
  thread per open connection beside the file pool.
- **`Sim`**: every test of `blaze-remote-proto`, `blaze-http` and the gateway's logic, under fault
  rates and crashes; the test that a seed replays is the existing one's shape.
- **`Uring`**: the bulk `/get` reads and the pack appends on one ring, behind a measurement in
  slice 8 showing it beats `Threads` on a GitHub runner. The daemon is not inside the test
  sandbox, whose seccomp filter refuses `io_uring_setup`
  ([cargo.md](cargo.md#sandboxed-tests-an-experiment-opt-in)), so nothing stops it there.
- **HTTP/3** to the gateway's domain, as `quiche` over `SendTo`/`RecvFrom` on any of the three.
  Real work (connection migration, pacing timers through `Timer`); worth it only if slice 8 shows
  it helps on lossy or long links (users far from a Cloudflare location), since a datacenter
  runner is unlikely to gain.

Crates to reuse, and not:

- **Reuse**: `sha2` 0.11; `zstd` with `zdict_builder` for writing and training (0.13.3 in the
  workspace today, 0.14 current, **reported**); `ruzstd` for reading inside the Worker, where the C
  library is awkward in WebAssembly (whether it reads every frame `zstd` writes with a dictionary is
  checked by a property test, **unverified**); `ed25519-dalek` (2.2, already used by
  `remote/signed.rs`); `hmac` and `hkdf`; workers-rs (`worker`) for the Worker; `rusty-s3` 0.10
  (sans-IO S3 request signing) for the copies and the self-hosted S3 store; `rustls` (unbuffered
  API) and `httparse`; `quiche` behind its slice.
- **VectorCDC**: no crate on crates.io (**reported**). ix has one, `crates/storage/cdc` in
  indexable-inc/ix: AE with VectorCDC's SIMD, AVX-512, AVX2, NEON and a scalar reference that the
  tests compare against, defaults 16/64/256 KiB, no dependencies beyond `bytes` and `parking_lot`
  (**read**). Port it as `blaze-cdc` rather than depend on the ix workspace.
- **Not `fastcdc`**: a good crate (5.0, used by Attic and REAPI's reference), but the decision is
  VectorCDC, and AE needs no table. **Not `gearhash`**: VectorCDC is not a gear hash.
- **Not tokio, hyper, compio or tokio-quiche** in the new crates: each brings an IO layer or
  runtime beside `blaze-core`'s drivers, which is what the sans-IO core exists to avoid.
- **Not `aws-sdk-s3`** in the new crates: it does its own IO, and no client talks S3 any more.
  Today's `s3.rs` keeps it until slice 3 replaces it.
- **Not `object_store`**: it does its own IO as well.

## Rollout

Ranked by what each gives. Each slice lands on its own and is measured.

1. **The gateway over R2, serving today's remote.** The bucket (made); the Worker v0 on the
   custom domain, holding the R2 binding and the R2 secrets (deleted from the repository); a
   `blaze` write token in `cache-writer`. v0 serves today's layout under `v3/`: `GET` of packs,
   key lists and manifests for anyone, and `PUT` for the write token, where it hashes each pack
   and key list against its name. A new `Place::Gateway` in `remote.rs` reads and writes it; CI
   on main pushes with today's `blaze-engine cache push`, and every CI run reads. Metering from the
   first request. First run of a real object store; measures one stream and 16 through a Worker
   on our runners, and the latencies of [Cost and latency](#cost-and-latency). No format change,
   and no expiry: `v3/` is removed by hand once slice 4 has replaced it.
2. **The codegen version.** `blaze-semver`, `Identity` without `build`, the golden check and the
   determinism check in the merge gate. Removes the per-release miss. Measures the corpus's keys
   against nushell's.
3. **Sans-IO crates, the new packs and the API.** Needs #283 merged. The socket effects in
   `blaze-core` (`Threads` and `Sim` first), `blaze-cas`, `blaze-pack`, `blaze-gateway-api`,
   `blaze-http`, `blaze-remote-proto`, `blaze-gateway-core` with the Worker and the native server;
   `/has`, `/get`, `/put`, the location table, public and private namespaces; store format 4; the
   prefetch at daemon start, planned from today's key lists until slice 4's heads replace them.
   Measures fetch time and D1 cost against slice 1.
4. **The signed index.** Segments, project and unit heads, key lists, the root and CI keys, the
   tenant field and the index-write checks; manifests and key lists of today retired. Unit heads
   let other projects use what blaze's CI built. The refresh inside head writes, then the
   lifecycle rules with `wrangler r2 bucket lifecycle add`.
5. **Large files.** `blaze-cdc` from ix; dependency units and the toolchain as chunk lists;
   measures dedup across consecutive main commits.
6. **Compression.** The upload level and the dictionaries, measured against level 3 in 128 KB
   blocks, on blaze's workspace and nushell.
7. **Test passes.** Slice R2 of [ci-runner.md](ci-runner.md) writes its records into this index;
   flake entries; `BLAZE_TESTS=affected` reads them, so a branch skips what main passed.
8. **Drivers.** The socket effects on `Uring`, and HTTP/3 over `quiche`, each kept only if it
   measures faster where it is meant to.
9. **Private tenants.** Registration (tenant id, root key), tokens, per-tenant limits, the
   tenant list in `BLAZE_REMOTE`, and the threat model's examples as end-to-end tests: a second
   tenant probing the first's planted secret, fixture and unit head gets only 404s, with timing
   that does not separate them.

The refresh (slice 4) must ship before the lifecycle rules are added (its last step), or the
first rule would expire what a head names.

## Open questions

- **The VFS's hash** (for #283's authors): move `blaze-vfs` from BLAKE3 to SHA-256 now, while
  the journal persists no hashes, so a file node's hash is its CAS name? Proposed: yes
  ([Hash](#hash-sha-256)). If not, the store keeps a BLAKE3-to-SHA-256 alias per file node.
- **Sockets in `blaze-core`**: new `Effect` variants (proposed), or a `Driver` generic over its
  effect type so the network lives outside `blaze-core`?
- **Is reading the shared tier on by default** for released blaze builds? On by default is what
  makes everyone benefit; it also tells Cloudflare and us which dependency units a machine builds
  (the unit keys it asks for, and the address it asks from). Proposed: on, with `BLAZE_REMOTE=off`,
  and said in the install docs.
- **Should an unreachable tier stop a local build**, as it stops CI? Proposed: no, with the status
  on every `Finished` line; the no-fallback rule could be read either way.
- **Per-head or per-entry signatures.** The design signs each head, which covers every entry once.
  Per-entry signatures would let one entry be served alone, at 64 bytes and one verification each.
  Is per-head what you meant by "each entry signed"?
- **The cache's domain**: which name, and on which Cloudflare account (`cache.<domain>` above).
- **Who deploys the Worker**: by hand with `wrangler deploy`, or a `cache-deploy` environment on
  main holding a Cloudflare API token. Proposed: the environment, so the Worker's code is what
  main says.
- **D1 cost**: about 2 rows per new blob, free within the plan's 50 million rows a month (about
  25 million blobs), then about $0.05 to $0.15 per main commit and $1.60 per cold fill (above).
  Acceptable, or should slice 3 go straight to one row per run of blobs?
- **Anonymous limits for GitHub runners**: fork pull requests read anonymously, and GitHub's
  runners share address ranges, so a per-IP limit may throttle many unrelated runs at once. Fork
  runs get no secrets and no OIDC token to tell us who they are. Proposed: a generous per-IP
  byte limit, watched through the meter.
- **Which projects are public tenants** besides `blaze`, and who may create one: public blobs are
  readable by anyone, so a tenant must only be public if everything it builds is open source.
- **Edge-cache timing for public blobs** ([threat model](#threat-model)): accepted as is, or
  should a private tenant's reads of public blobs bypass the edge cache?
- **Encrypting private packs at rest** under a key derived from `k_t`, so a leaked R2
  credential reads nothing of a private tenant ([Tenants](#tenants-namespaces-and-tokens))? It
  costs the edge cache's sharing of windows and a decrypt per read. Proposed: before the first
  outside private tenant, not before.
- **Billing**: what a private tenant is charged for (bytes stored, bytes served, D1 rows), read
  from the meter.
- **Who holds the root key**, and where (a hardware key, a printed backup).
- **Dev builds and the version**: should a blaze built from a dirty tree salt its local keys with
  a hash of the codegen crates, so a developer's codegen experiment never lands in their own
  store under the released version's keys?
- **Sampled rebuilds on clients**: should a client be able to recompile a fraction of what it
  fetched and compare (`BLAZE_REMOTE_VERIFY=sample`), for users who trust CI less than we do?
  Needs per-function determinism, which the golden check gives.
- **Windows and macOS**: CI on main runs Linux x64 and arm64 and macOS arm64 (`train.yaml`,
  **read**). Each target is its own set of heads; is macOS arm64 filled from the start?
