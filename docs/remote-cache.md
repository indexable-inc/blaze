---
status: current
summary: The remote cache, an option that is off unless chosen: what crates/blaze-remote does today (the read path from a named origin, the published set and its publish, a bucket push and pull no command calls) and its settings.
---
# The remote cache: an option, off unless chosen

A new checkout is served by the machine's own content-deduplicated store: every checkout on the
machine reads and writes the one store under the home ([gc.md](gc.md)). Publishing records to an
origin and fetching them from one is built in `crates/blaze-remote` and is off unless chosen. With
no `--origin` named, a command reads nothing but the local store; with no `remote.json`, or
`"enabled": false`, nothing in the crate contacts anything (a test counts zero requests at the fake
bucket). Nothing is designed around the remote or traded against the ranked goals (AGENTS.md).

A fetched record is byte for byte what a local build makes, so a miss, an unreachable origin and a
refused byte all end in the same local build, which is the record's definition. What is never done
is a second try, another origin, or a refused byte used anyway: each is a typed error naming the
object and the key.

## What is built

### Reading from a named origin

`--origin <folder | http(s)://base> --origin-key <64 hex>` after the checkout, both or neither
(blaze-cli `store.rs`, `NamedOrigin`). A key the local store misses is asked of the origin by byte
range; every byte is checked against a digest chain that starts at a head signed by the named key
before it is put. What is not fetched is a typed miss with one stderr line, and the record is
computed locally. An `https://` origin goes through blaze-cli's `fetch` feature. The objects, the
trust steps and the reader: [remote-read.md](v2/plan/design/remote-read.md).

### The published set

The toolchain's records (std, core, alloc) for one toolchain and target, laid out by
`published::lay_out` under `public/<toolchain>/<target>/` and read in at most three waves
([published.md](v2/plan/design/published.md)).

- **Publish.** `blaze store publish <folder> --signer <key file>` (`store/publish_command.rs`), only
  in a blaze built with blaze-cli's `publish` feature, which is off by default. A developer's blaze
  refuses it by name and writes every record `LOCAL`; an `http://` or `https://` origin is refused
  for everyone.
- **Take.** A home with `--origin` naming a set takes it whole, once, before the command's first
  phase (`store/take.rs`), each segment checked once against its row; the head is asked at most once
  per ten minutes. `trust::PUBLISHED_KEYS` is empty, so a set is believed only under an
  `--origin-key` named for it.

### The bucket: push and pull, called by nothing

`bucket`, `push`, `pull`, `shard`, `bundle`, `trust`, `s3`, `http`, `http3` and `cmd` speak to an
S3 bucket (R2) directly: plain-store segments byte for byte, each with a signed 200-byte statement;
index shards; a signed root per namespace and toolchain, replaced by compare-and-swap; bundles naming
the segments a first build needs; key lists signed by an anchor key. The layout, the round trips
and the trust chain are `crates/blaze-remote/src/lib.rs`'s doc. `cmd` holds `blaze cache push |
pull | status | keys`, but blaze-cli dispatches none of them, so no command reaches this path.

Also in the tree and called by nothing: `crates/blaze-pack` (transfer packs) and `tools/remote-cache`
(a Cloudflare Worker gateway; no client in the tree speaks to it).

## Settings

The bucket path's settings (`crates/blaze-remote/src/settings.rs`), read once at the entry point:

- `remote.json`: `enabled` (true when the file exists and omits it), `bucket` (`endpoint`, `name`,
  `public_url`, `cas`: whether the bucket honours conditional puts, `transport`: `"h1"`, the
  default, or `"h3"` for anonymous reads of the public address, with no fallback between them),
  `namespaces` (each a `name` and its `anchors`, hex Ed25519 keys), `root` (the root's name) and
  `sample_per_1024` (the share of pushed keys checked against the bucket for determinism, 64 by
  default). Unknown fields are refused.
- `remote-credentials.json`, owner-only (mode 0600, refused otherwise), for a machine that writes:
  `access_key_id`, `secret_access_key`, `signing_seed`, `key_list`.
- In CI the same come from the environment: `BLAZE_CACHE_ENDPOINT`, `BLAZE_CACHE_BUCKET`,
  `BLAZE_CACHE_PUBLIC_URL`, `BLAZE_CACHE_PUBLIC_ANCHORS` (comma-separated hex),
  `BLAZE_CACHE_ACCESS_KEY_ID`, `BLAZE_CACHE_SECRET_ACCESS_KEY`, `BLAZE_CACHE_SIGNING_SEED`,
  `BLAZE_CACHE_KEY_LIST`, `BLAZE_CACHE_TRANSPORT`. A value set both in a file and in the
  environment is refused, naming both.

Secrets are held as `Secret`, shown as `<redacted>`; an error about one names the setting, never
its value.

The gateway's design and the packs' (the sections `tools/remote-cache` and `crates/blaze-pack`
cite) are in git: `git show f602cdc99f:docs/remote-cache.md`.
