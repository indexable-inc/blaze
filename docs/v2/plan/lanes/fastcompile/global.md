---
status: plan
summary: blaze v2 fastcompile idea 8, one global cache (crates/blaze-global): which keys are public, how fetched values are trusted (Ed25519 records by a pinned publisher, then the content hash), the pipelined, batched, chunked HTTPS protocol, the cold build as one signed bundle, and the hooks other lanes are asked for.
date: 2026-10-29
---
# fastcompile-global: one global cache

Idea 8 of [compile-time.md](../../design/compile-time.md). What depends only on public inputs
(crates.io crate versions, std, sandboxed proc-macro and build-script runs, dependencies'
front-end results and code) is computed once, by blaze's builders, and fetched by every machine instead of
built. The code is `crates/blaze-global/`; the boundaries are in [notes.md](notes.md).

## What is public (`public.rs`)

A store `Key` is a hash, so nothing in it says what it was made from. Publicness is judged where
a key is made: a `Rule` takes how the key was made (`QueryMade`, `RunMade`, `FunctionMade`,
`DevFunctionMade`, a side table of a public function, a `DerivedMade` from a public front-end
value), makes the key again with its owner's own function (`Key::of`,
`blaze_usercode::index`, `blaze_codegen::query::keys`, `blaze_diagnostics::cache`,
`hir_ty::consteval::deps`), and checks every input's `Origin`. What passes is a `Public`,
which no other code can make, and the client fetches and publishes only those. The client
asserts that every key the store asks it for was shown public.

- Public: a crates.io crate version by its lock checksum, the pinned toolchain's files, the
  builders' system image, and the outputs of public runs.
- Never public: a workspace member, a path or git dependency, another registry, a variable of
  the machine's environment read with a value, a run that read the whole environment or
  anything untraceable, or code compiled for more than its target's baseline.

**Front-end results of a dependency** (`Rule::derived`, request 00842 from cold-v2). A fetched
image is followed by the crate's check, its constants, goals and MIR. Those built from a public
image key are public too:

- the bodies part of its check (`cache::bodies_key` of the image, its dependencies' check keys,
  each a `Public`, and the borrow checker's mode), whose key is the crate's check key;
- its check outcome (`cache::key` of the check key and the lint flags and mode, `Inputs`);
- the values of its constants (`consteval::deps::key` of the check key and the constant's
  stable key).

The flags, the mode and the item key are settings and names in the key, not inputs read from
the machine; the outcome's texts hold the package directory and `OUT_DIR` as placeholders, not
paths. Two kinds are not covered yet: goal answers (`next_solver::goal_store`) and the item
store's records (`body_store`, kinds 0x3800 to 0x3840). Their keys are made in private modules
(`record`, `stable`), and a goal's answer is public only when every fact it read is of a public
crate. Lane db was asked for the key functions and each fact's crate (fastcompile-queue 3).
Until they are given, the builders don't publish these two kinds.

**Dev functions** (`Rule::dev_function`). Cranelift's and copy-and-patch's code is kept under
`keys::template(stored(instance, built_in))`, with no import decisions. It is public on the
same terms as release code (pinned toolchain, public code and type arguments), and only when
compiled for the target's baseline (`Toolchain::features` empty: a `Runs::Elsewhere` build
with no `-C target-cpu` or `-C target-feature`). Code tuned to the builder's CPU has a key of
those features, which no other machine's build asks for. Nondeterminism is no reason to keep
dev code private. Cranelift and the stencil library give the same bytes for the same input,
and the local store already reuses a template by this key, so a template that depends on
anything outside its key is a bug in the local cache first. A cold dev build is
`Runs::Here`, so its keys are of the host's features. It finds published code only when the
dev driver asks for baseline code of dependencies (cold-v2's side).

`Secrets` holds the publisher's seed (raw and as hex) and every non-public environment value. A
value that contains any of them is refused before it is signed (`Client::publish`), and the
error names where the secret came from, never the secret.

## Trust (`sign.rs`)

A content hash proves the bytes but not that the key maps to them, and the cache is shared. Each
value has a `Record` (key, kind, content hash, length), signed with Ed25519 by a publisher key
this blaze pins (`sign::PINNED`). Ed25519 comes from `ring`, already a workspace dependency, so
no signature crate is added.

A fetched value enters the store only after two checks, in order:

1. The record's signature by a pinned key. Nothing is allocated for a value whose record fails
   it: the value is read past, so the chunk windows stay in step.
2. The bytes against the record's content hash.

A value that fails either check is answered `Done::Failed` with an error naming the key, so the
store reports `Fetched::Refused` and never uses it. It is never asked of anyone else.

Every count and length from the network is checked against what the body holds, or a limit,
before anything is allocated. A key's value never changes, so a replayed record is the same
record: there is no expiry or revocation to check.

## The protocol (`client.rs`, `server.rs`, `wire.rs`, `http.rs`, `tls.rs`)

HTTP/1.1 with `Content-Length` bodies, over rustls driven sans-IO through blaze-core's `Connect`,
`Send`, `Recv` and `Listen`.

- **Batching and pipelining.** One fetch names up to 256 keys of unknown length, or 4096 keys a
  manifest lists. A connection carries 4 requests at once. The client asks one request a
  connection in turn, and opens as many connections as the queued requests fill, at once, up to
  16. A bundle opens 4 while its manifest is asked.
- **Response bounds.** A response takes its first value whatever its length, then further
  values while it stays under 8 MiB. Keys past that are answered *deferred* and asked again.
- **Chunked values.** Values travel as the store's FastCDC chunks. Each direction of a connection
  keeps the last 32 MiB of chunks, and a chunk seen again goes as a 9-byte reference, so
  near-equal values (the same crate rebuilt with one change) share bytes on the wire.
- **The client.** It is the store's `Remote`, so a fetched value arrives as the completion of
  `Store::fetch`. The client checks the signed record. The store hashes the bytes once against
  the record's content, as it does for any remote, and keeps nothing that differs.
- **The server.** It keeps each value and, under `record_key`, its signed record, putting the
  value first. It checks a publish as a fetch is checked, and answers only once the puts are
  durable.

A malformed, oversized or unknown request is answered with its status and reason, and the
connection is closed.

## The cold build as a download (`bundle.rs`)

`Locked::parse` takes the lock file's crates.io packages that the build builds, with their
features. `BundleSpec` adds the toolchain, target and settings, and `BundleId` hashes all of it.
The builders publish every value of the cold build and then a `Manifest` with every record,
signed once. The server keeps a manifest only when it has the value and the identical record of
every entry.

`Download` asks for the manifest and checks its signature, then fetches everything listed that
the store lacks, in one `fetch_many`, spread across the connections. Each listed value is checked
against its listed record and its hash, with no per-value signature check.

## Tests (`tests.rs` and each module's own)

The simulation runs a client, the machine's store, and either a real server on a store of its
own or a server the test plays by hand. It covers:

- **Round trips:** fetch, deferral and re-asking, batching into 4 requests for 1000 keys, and
  publish.
- **Refusals:**
  - an unpinned key, a damaged signature, a content mismatch, or a record of another key;
  - damaged, truncated, reordered, short, oversized, chunked or non-HTTP responses;
  - unknown answers, piece counts past the body, oversized pieces, and hang-ups before and
    mid-response;
  - error statuses, and an unreachable cache.
- **Bit flips and cuts:** a flipped bit or a cut anywhere never gets a value kept with other
  bytes.
- **Secrets:** a value holding one is never sent.
- **Requests no client sends:** 405, 404, 413, 411 and 400.
- **Bundles:** the round trip (a key already present is not fetched), a missing bundle, a
  manifest by an unpinned key, a listed value with another record, and a manifest naming values
  the cache lacks.

The publicness rule is tested per input kind in `public.rs`.

## State of the art, matched or beaten

| system | what it does | here |
|---|---|---|
| Bazel remote cache, BuildBuddy, EngFlow (REAPI) | action-keyed CAS shared by a team; trust is "who can write" (auth) | the same action-keyed sharing, but trust is per value (a signature by a pinned builder key), so anyone may serve or mirror it and a write credential leaking can't poison anyone |
| sccache, ccache with a remote | compiler-invocation-keyed cache for a team | a key covers what rustc's command line misses (traced env, files read); sharing is global and limited to public inputs, decided by a checked rule rather than path heuristics |
| Nix binary caches (`narinfo` signatures, `trusted-public-keys`) | signed store paths from cache.nixos.org | the same model (Ed25519 by pinned keys, content-addressed values); beaten on granularity: per query, per build-script run and per optimized function, not per package |
| casync, desync, zsync | content-defined chunking for transferring trees | the store's own FastCDC chunks travel on the wire, so chunk identity is the store's, and nothing is re-chunked |
| rsync, zstd `--long` / dictionary windows | send what the peer lacks | a 32 MiB per-connection window both ends evict the same way, so references need no round trip |
| crates.io / docs.rs prebuilt artefacts (none for Rust builds today) | — | the cold build of a lock file's crates.io dependencies is one signed manifest and a parallel download |

## Hooks asked of other lanes

See `v2/requests/fastcompile.md`, `fastcompile-global` 1 to 4:

| lane | hook | why |
|---|---|---|
| store | make `content_of` and `chunk::chunks` public in `blaze-cas` | records name the store's own content hash, and the wire's pieces are the store's chunks |
| codegen | make `driver::stored` public | `Rule::function` remakes the release key |
| cargo | the daemon calls `Client::fetch` on a public miss and `Download` at cold start, and routes completions | the narrowest hook: the store's own `Remote` slot |
| ci | the builders' publish job, the publisher key as a secret, `sign::PINNED`, and the server with TLS | the publish side of the cache |

## Speed of light

Three paths are hot: deciding that a key is public (once a lookup), a fetch (round trips, bytes
on the link, and the content-hash check), and the cold build served as a download (bound by the
link). The machine is the one in
[link/speed-of-light.md](../link/speed-of-light.md#the-machine): 3.5 GHz, BLAKE3 at 6 GB/s a
core (AVX-512), a copy out of cache at about 12 GB/s, a syscall at 0.15 µs. The figures for the
links, TLS and Ed25519 below are assumed, not measured here.

### The work that can't be skipped

- **Publicness.** A query is public when each input's origin is. That is one `Key::of` over its
  inputs, which the key needs anyway, and one compare per input. A run is public when each path
  it read is under a public root. That is one walk over the path's components, the same walk
  `Path::starts_with` does, plus a probe per depth that some root has. A function is public when
  its instances are. That is the instance keys, which make the release key, each hashed once.
- **A fetch.** For each value the client must check the record's signature, which is what makes
  a mirror trustworthy. The bytes must cross the link and be decrypted (TLS). The value must be
  hashed once against the record's content, which is what makes the value trustworthy. Then it
  is handed to the store. Values a bundle lists are covered by the manifest's one signature, so
  they skip the per-value verify.
- **A cold download.** Connect and TLS 1.3 cost 2 round trips, the manifest 1, and the first
  fetch 1. After that the time is the bundle's bytes over the link. One TCP connection carries
  at most its receive window per round trip.

### Unit costs

| step | cost |
|---|---|
| BLAKE3 of bytes, one core | 0.17 ns/B |
| a copy of a buffer | 0.08 ns/B |
| AES-GCM open, one core (assumed) | 0.2 ns/B |
| Ed25519 verify, `ring` (assumed) | 40 µs |
| `Key::of` over \(n\) inputs (one BLAKE3 chunk up to 29) | about 100 ns |
| one path component, parsed and hashed | about 3 ns |
| a binary-search probe into a few hundred roots | about 10 ns |
| a `Path` compare of \(d\) components | about \(5d\) ns |
| the dev link (assumed 1 Gbit/s, 20 ms RTT) | 8 ns/B |
| the CI link (assumed 10 Gbit/s, 1 ms RTT) | 0.8 ns/B |
| one connection's TCP receive window (Linux autotuning, assumed) | about 6 MiB |

### The bounds

- **A query:** \(T_q \ge t_{\text{Key::of}} + n\,t_{\text{cmp}} \approx 100 + 2n\) ns.
- **A run** with \(F\) traced paths of \(d\) components and \(p \le 3\) root depths on each:
  \(T_r \ge F\,(3d + 10p + 5d)\) ns, about 100 ns a path. The run's given and traced bytes are
  hashed once for its key, which the key needs anyway.
- **A function** with \(k\) instances: \(T_f \ge (k + 3)\,t_{\text{Key::of}}\), meaning each
  instance key once plus the stored key, the blob key and the release key.
- **A fetch**, per byte on the client:
  \(c \ge c_{\text{TLS}} + c_{\text{rustls copy}} + c_{\text{assemble}} + c_{\text{hash}} = 0.2 + 0.08 + 0.08 + 0.17 = 0.53\)
  ns/B, about 1.9 GB/s. A piece the window hasn't seen costs one more copy into the window, which
  gives 0.61 ns/B. A per-key value adds 40 µs for its signature.
  The time for \(N\) values of \(B\) bytes in all is \(T \ge \max(B/\text{link},\ B\,c) + N_{\text{unlisted}} \cdot 40\,\mu\text{s} / \text{cores} + \text{RTT}\).
  Both links assumed here are slower than 0.61 ns/B, so the link sets the bound.
- **A cold download** of \(B\) bytes over \(L\) connections:
  \(T \ge 4\,\text{RTT} + B / \min(\text{link},\ L \cdot \text{rwnd}/\text{RTT})\). At 10 Gbit/s
  and 50 ms with 6 MiB windows, one connection carries 120 MB/s, so filling the link takes 11.

### Today's code, counted from the code

| path | before | now | bound |
|---|---|---|---|
| a run's path | a `starts_with` against every root, \(R \times 40\) ns (12 µs at \(R = 300\)) and a `PathBuf` per absolute value | one walk and the probes, about 100 ns, with no allocation (`Rule::path`) | about 100 ns |
| a query | an allocated key list | `Key::of` over the caller's slice, no allocation | at the bound |
| a function | each instance key hashed twice and a `Vec` of keys | each hashed once, up to 16 kept inline on the stack | at the bound |
| fetch, client CPU a byte | TLS 0.2, rustls copy 0.08, plain-buffer regrowth 0.08, window 0.08, assemble 0.08, the client's own hash 0.17, framing copy 0.08, the store's hash 0.17: 0.94 ns/B, 1.06 GB/s | the client's hash gone (the store's is the one check), the buffer sized to the response once: 0.69 ns/B, 1.45 GB/s | 0.61 ns/B |
| fetch on the 10 Gbit/s link | CPU-bound at 1.06 GB/s, 85% of the link | link-bound | link-bound |
| connections | one opened at a time, only once every open one held 4 requests: the fourth was up at 9 RTT; link 0 took the first 4 requests | as many as the queued requests fill, opened together, up to 16; requests asked one a connection in turn | 4 RTT to the first byte |
| a bundle | the first connection only once the manifest was asked | 4 connections opened while the manifest is asked | — |
| a high-latency link (10 Gbit/s, 50 ms) | 4 connections \(\times\) 6 MiB \(/\) 50 ms = 480 MB/s, 38% of the link | 16 connections: 1.9 GB/s, link-bound | link-bound |
| a 400 MB bundle on the CI link | about 386 ms (CPU-bound, plus 9 RTT) | about 325 ms | 324 ms |
| buffering | 64 MiB responses, 4 requests a connection: 1 GiB in flight | 8 MiB responses: 512 MiB at most over 16 connections | — |

Batches of 256 keys are what make the round trips small. A cold build of a few thousand
values takes about \(N / (256 \cdot 4)\) round trips spread over the connections, not \(N\).

The steady state allocates nothing. A held value goes through the window, a warm window takes
new pieces, a fetch is parsed onto a frame, and a path is classified, all with no allocation.
`blaze-alloc-count` budget tests cover each of these in `wire.rs` and `public.rs`.

### The gap still open

- **The framing copy**, 0.08 ns/B, puts the client at 0.69 rather than 0.61 ns/B. `Answer::encode_framed` copies the value
  into the completion's frame. blaze-cas would need an API that frames a value in the buffer it
  was assembled in (requested as `fastcompile-sol-global 1`). On both links assumed here the
  link is the bound, so it costs nothing there today.
- **The per-value signature.** A per-key value costs 40 µs of one core to verify. At 10 Gbit/s
  that is the link time of 50 KB, so a fetch of many small values not listed in a bundle is
  bound by Ed25519 rather than the link. The fix keeps per-value trust: the publisher signs one
  Merkle root over a batch of records, as Certificate Transparency (RFC 6962) and TUF do, and each record carries
  its inclusion path. Verify then costs 40 µs a batch plus about \(\log_2 b\) BLAKE3 hashes a
  value. It needs a new signed-record format, so it is left for the publish job's lane to adopt.
  A bundle already works this way, with one signature over the whole manifest.
- **The server** chunks and hashes each value on each serve, about 0.42 ns/B before TLS, so one
  server core feeds about 1.4 GB/s, one 10 Gbit/s client. Serving many clients takes a server a
  core (one listener each), and keeping the store's chunk boundaries would save the re-chunk.

### Against the state of the art

| system | what it does | here |
|---|---|---|
| Bazel REAPI (`FindMissingBlobs`, `BatchReadBlobs`, ByteStream over HTTP/2) | batches of digests over multiplexed streams; the client checks each blob's digest | the same batching (256 or 4096 keys) over pipelined HTTP/1.1 and parallel connections; one hash a value, not one on the client and one in the CAS |
| Nix (`narinfo`, `nix copy` with `http-connections` = 25) | a signature a store path and parallel connections to the cache | the same per-value Ed25519 and parallel connections, sized to the work; a bundle needs one signature for all of it |
| casync, desync | chunks fetched by content hash in parallel | the same, with a window that sends a repeated chunk as a reference and needs no round trip |
| Docker registry (OCI distribution) | layers fetched in parallel, checked by digest | a bundle is a lock file's whole dependency build behind one manifest |
