# blaze-gateway API: the blob store (slice 3 of docs/remote-cache.md)

The content-addressed store beside v0's per-tenant layout (API-v0.md): `/has`, `/get`,
`/blob/<sha256>` and `/put` on the gateway's root; and the signed index of slice 4 under
`/t/<tenant>/`: key lists, heads and segments ([Index](#index)).

A blob is named by the lowercase hex SHA-256 of its **uncompressed** bytes, at most 4 MiB of them.

## Callers and namespaces

Auth is v0's: `Authorization: Bearer <token>`, the KV records of API-v0.md, 401 for an unknown or
expired token, never a fall back to anonymous. The namespaces a request may read are fixed from
the token before any lookup:

| caller | reads | `/put` writes into |
|---|---|---|
| anonymous | public | 401 |
| a public tenant's token | public | public (write token), 403 (read token) |
| a private tenant's token | its own, then public | its own (write token), 403 (read token) |

The public namespace's keys are blob hashes. A private tenant's keys are `HMAC-SHA256(k_t, hash)`,
`k_t = HKDF-SHA256(ikm: GATEWAY_SECRET_<v>, salt: the tenant's 32-byte salt, info: "blaze tenant
key" 0x00 <tenant id> 0x00 <v>)`, with the salt and `v` in D1's `tenants` row (made at the
tenant's first use, salt from SQLite's `randomblob`). A hash another tenant stored privately is
absent to everyone else by the same code path an unknown hash takes.

## Bodies

Requests and answers other than pack streams and blob bytes are deterministic CBOR (RFC 8949
section 4.2.1: shortest integer forms, definite lengths, map keys in bytewise order of their
encodings), `content-type: application/cbor`. A hash in CBOR is a 32-byte byte string.

## Pack stream

`/put`'s body and `/get`'s answer, `content-type: application/vnd.blaze.pack-stream`:

    magic    8 bytes  "BLZSTRM1"
    record*  one of:
      0x01 blob     hash[32] flags[1] (dict[32] when flags bit 2) ulen[u32 LE] clen[u32 LE] data[clen]
      0x02 missing  hash[32]                                    (/get only)
    0x00 end                                                    (a stream without it was cut short)

`flags` bits 0-1 are the codec: 0 is one zstd frame, 1 is raw bytes (`clen == ulen`). Bit 2 says a
dictionary hash follows; the gateway refuses dictionaries until slice 6 (422). Other bits are 0.
`ulen` is at most 4 MiB.

## Requests

| request | answer |
|---|---|
| `POST /has` `{"hashes": [h, ...]}` (at most 20,000) | 200 `{"state": [s, ...]}`, one per hash in order: 0 absent, 1 public, 2 in the caller's own namespace (also when public) |
| `POST /get` `{"hashes": [h, ...]}` (at most 20,000) | 200, a pack stream: a `blob` record for each hash found, in the order the gateway reads them (own namespace first), then a `missing` record for each other hash, then `end`. The records are the stored frames, as `/put` received them |
| `GET` / `HEAD /blob/<h>` (optional `Range: bytes=a-` or `bytes=a-b`) | 200 / 206: the stored frame (zstd or raw), `x-blaze-codec: zstd\|raw`, `x-blaze-ulength`; the one 404 when absent from every namespace the caller may read; 416 |
| `POST /put` a pack stream (at most 32 MiB, `Content-Length` required) | 200 `{"pack": "<hex>", "blobs": n}` after the pack is stored and every location recorded |

`/put` decompresses every zstd frame in the Worker (workerd's `node:zlib` zstd, the C library in
the runtime) and refuses the whole request, storing nothing, when a frame does not hash to its
name, its length is not its `ulen`, a `ulen` is over 4 MiB, or a frame carries a dictionary: 422
with the first offending hash. A malformed or truncated stream is 400. Every accepted `/put` is
stored in full as one new pack, even when every blob in it is already present, so neither its
answer nor its timing depends on what is stored. Each `(namespace, key)` has one location, the
newest copy.

## Storage

A `/put` becomes one object, `cas/pub/<sha256 of the pack>.bpk` or, for a private tenant,
`cas/t/<tenant id>/<HMAC(k_t, sha256 of the pack)>.bpk`:

    header  "BLZPACK1"
    frames  each blob's data, back to back
    index   CBOR array of [key, offset, clen, ulen, codec], key = the namespace's key for the blob
    footer  index offset [u64 LE], index length [u64 LE], "BLZPACK1"

D1 (`migrations/`): `tenants(id, salt, key_version)`, `objects(object, uploaded)` (R2's upload
date, unix seconds) and
`blobs(ns, key, object, offset, length, ulength, codec, dict)` keyed by `(ns, key)`, `ns` being
`pub` or the tenant id, `key` the hex key. A private pack's object name, its index and its D1
rows hold HMACs only, never a blob's hash.

A location whose object's `uploaded` is more than 80 days old is absent to every request (the
object expires at 90). Each `/put`, segment put and head write then deletes up to 500 such rows
and the `objects` rows left without one; there is no other job.

## Index

Per tenant: one key list, heads, and segments. Readable as v0's layout is: a public tenant's by
anyone, a private tenant's by its own tokens (read or write); to anyone else a private tenant's
key list, heads and segments are the one 404 an absent one is. Writes need the tenant's own write
token. All bodies are deterministic CBOR as above; integers are unsigned, times are unix seconds.

### Signed envelope

A key list and a head are each stored and served as the envelope the client sent:

    envelope = {"body": bstr, "sig": bstr(64)}
    sig      = Ed25519(secret key, context || body)
    context  = "blaze keys v1" 0x00   for a key list
               "blaze head v1" 0x00   for a head

`body` is the deterministic CBOR of the map below, and is what is signed, byte for byte. The
context keeps a key-list signature from passing as a head's and back. Every map has exactly the
fields listed (a missing, extra or mistyped field is 400).

### Key list: `GET` / `PUT /t/<tenant>/trust`

    body = {
      "tenant": text,      the tenant's name in the URL
      "turn":   uint,      above the stored list's
      "signer": text,      the tenant's root key name
      "keys": [ {"name": text, "key": bstr(32) Ed25519 public, "role": "heads",
                 "not_before": uint, "not_after": uint (> not_before)} ]   at most 32, names unique
    }

The tenant's root public key is registered with the tenant (the KV record's
`"root": {"name", "key": hex}`), by its holder; the gateway never makes one. `PUT` checks the
signature against it, `tenant` and `signer`, and `turn` above the stored one (409 otherwise), and
writes conditionally on the list it read (412 if another write won). A tenant with no root is 403
with the reason. `GET` returns the stored envelope, `etag`, `cache-control: public, max-age=60` for
a public tenant. Object `trust/<tid>/keys`; it never expires. Answer to `PUT`: `{"etag": text,
"turn": uint}`. Revoking a key is a list at the next turn without it.

### Segment: `PUT /t/<tenant>/seg`, `GET /t/<tenant>/seg/<sha256>`

    segment = {
      "v": 1,
      "entries": [ [kind: text, key: bstr, blobs: [[sha256: bstr(32), ulen: uint], ...], data: bstr], ... ]
    }

Entries are strictly increasing by (UTF-8 bytes of `kind`, then `key`), compared bytewise. `blobs`
are what the entry's value is made of; `data` is the kind's own small value (may be empty). At most
4 MiB. `PUT` refuses (422, naming the first) a segment with a blob the tenant cannot read (public,
or its own private namespace; the one D1 query of `/has`) or whose `ulen` is not the stored one,
and a malformed one (400). It is named by the SHA-256 the gateway computes over the body and put
only if absent, so no write can replace a segment. Answer: `{"seg": bstr(32)}`. `GET` serves the
bytes as stored (`cache-control: public, max-age=604800, immutable` for a public tenant).

### Heads: `POST /t/<tenant>/heads`, `PUT /t/<tenant>/head/<name>`

A head's name is `p/<project>/<commit>` (`<project>` `[A-Za-z0-9._-]{1,128}`, `<commit>` 7 to 64
lowercase hex), `p/<project>/latest`, or `u/<unit key>` (64 lowercase hex).

    body = {
      "tenant": text, "name": text, "turn": uint, "created": uint,
      "versions": {text: uint, ...},       store, codegen, expand, script, unit, tests
      "toolchain": text, "target": text,
      "segments": [bstr(32), ...],         at most 128, unique
      "signer": text                       a key name in the tenant's key list
    }

`POST /t/<tenant>/heads {"names": [text, ...]}` (at most 500) answers `{"heads": [entry, ...]}`,
one per name in order: `null` when absent, else `{"etag": text, "head": bstr}`, `head` being the
stored envelope.

`PUT /t/<tenant>/head/<name>` takes an envelope (at most 64 KiB) and needs `If-Match: <etag>` (the
one read; quoted or bare) or `If-None-Match: *` for a new head (428 without). In order:

1. `tenant` is the URL's tenant and `name` the URL's name (422): a head cannot be replayed into
   another tenant or under another name.
2. `created` is at most 300 s ahead of the gateway's clock (422).
3. `signer` is in the stored key list (422 "unknown or revoked" otherwise), the gateway's clock is
   inside its `not_before`..`not_after`, and so is `created` (422).
4. The signature verifies (422).
5. With `If-Match`: the stored head is at that ETag (412 otherwise) and `turn` is above its turn
   (409). With `If-None-Match: *`: no head is stored (412).
6. Every named segment is in this tenant's index (422), and the refresh below.
7. The put carries the same condition to R2 (`onlyIf`), so of two writes from one ETag exactly one
   is stored and the other is 412: re-read, merge, sign and retry.

Answer: `{"etag": text, "refreshed": {"segments": n, "packs": n, "copied": n}}`, and
`server-timing: refresh;dur=<ms>`.

### Refresh

Inside a head write, before the head is stored (docs/remote-cache.md, "GC"): each named segment
whose R2 upload date is more than 2 days old, and each pack holding a blob those segments name
whose `objects.uploaded` is more than 45 days old, is written again onto its own name through the
R2 binding (a pack streamed back with its MD5 or SHA-256 checked by R2), then `HEAD`ed: the head
write fails (500) unless the upload date moved and the size and ETag are the same, and a pack's
new date is written to `objects.uploaded`. A private head never refreshes a public pack: the
public blobs it names in a pack past 45 days are copied (their frames, by range) into a new pack in
the tenant's own namespace, which its reads look in first; the public pack's date is untouched.
At most 64 rewrites or copies per request; past that the write is 503 with `Retry-After: 1` and
the same request goes on where it stopped.

### Objects

    trust/<tid>/keys
    index/heads/<tid>/<name>             private: HMAC-SHA256(k_t, "blaze head" 0x00 <name>), hex
    index/seg/<tid>/<sha256 hex>         private: HMAC-SHA256(k_t, "blaze seg" 0x00 <sha256>), hex

Lifecycle rules on the bucket expire `index/heads/` after 28 days, `index/seg/` after 32 and
`cas/` after 90; `trust/` and v0's `v3/` never.
