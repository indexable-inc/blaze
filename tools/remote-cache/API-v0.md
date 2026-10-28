# blaze-gateway API v0 (slice 1 of docs/remote-cache.md)

The contract between `blaze`'s `Place::Gateway` client (crates/blaze-cargo/src/remote/gateway.rs)
and the Worker in this directory. v0 serves today's remote layout (packs/, keys/, units/,
manifests/, deps/) per tenant. Slice 3 adds `/has`, `/get`, `/put` beside it (API.md).

Base address of a tenant: `https://<gateway>/t/<tenant>/v3`. The client is given that base
(`BLAZE_REMOTE=blaze+https://<gateway>/t/<tenant>/v3`, `blaze+http://` for local tests) and
puts objects at `<base>/<name>`.

## Names

Allowed `<name>`s (anything else is the same 404 as an absent object):

- `packs/<h>.pack`, `keys/<h>`, `units/<h>`: content-addressed, `<h>` = lowercase hex SHA-256 of the body.
- `packs/<h>.pack.zst`: `<h>` names the uncompressed pack; the body's own SHA-256 is sent as
  `x-blaze-sha256` on PUT, checked, stored, and returned on GET/HEAD as `x-blaze-sha256`.
- `manifests/<project>`, `deps/<project>`: mutable lists (signed by the client, not by the Worker),
  `<project>` matching `[A-Za-z0-9._-]{1,128}`.

R2 key: `v3/<tenant id>/<name>` (tenant id opaque, from KV, never reused).

## Auth

`Authorization: Bearer <token>`. KV `token:<sha256(token) hex>` → `{"tenant": "<name>",
"scope": "read"|"write", "expires": <unix s>|null}`. KV `tenant:<name>` → `{"id": "<opaque>",
"public": bool}`.

- Public tenant: GET/HEAD anonymous. Private tenant: GET/HEAD need a token of that tenant.
- PUT/POST need a write token of that tenant.
- No token where one is needed, or an unknown/expired token: 401. A read token on a write: 403.
- A private tenant's object asked for without its token, an unknown tenant, a bad name, and an
  absent object: the same 404, same body, same code path (authz decided before any R2 lookup).

## Requests

| request | answer |
|---|---|
| `GET <base>/<name>` (optional `Range: bytes=a-` or `bytes=a-b`) | 200 / 206, `ETag`, `Content-Length`, `Last-Modified`, `x-blaze-uploaded: <unix s>`; 404; 416 |
| `HEAD <base>/<name>` | the same headers, no body; 404 |
| `PUT <base>/<name>` (body ≤ 90 MiB) | 200 `{"etag": "..."}` + `ETag`. `If-Match: <etag>` / `If-None-Match: *` → 412 when not met. Content-addressed name whose body does not hash to it (or to `x-blaze-sha256` for `.pack.zst`): 422 `hash mismatch`, nothing stored. 413 over the size |
| `POST <base>/<name>?uploads` | multipart start (packs/, units/ only): 200 `{"upload": "<id>"}` |
| `PUT <base>/<name>?upload=<id>&part=<n>` | one part, n from 1, ≤ 90 MiB, every part but the last the same size and ≥ 5 MiB: 200 `{"etag": "..."}` |
| `POST <base>/<name>?upload=<id>` body `{"parts":[{"part":1,"etag":"..."},...],"sha256":"<hex>"}` (+ `x-blaze-sha256` for `.pack.zst`) | completes into a staging key, hashes it in the Worker, then stores it under the name only if the hash matches: 200 `{"etag": "..."}`; 422 (staging removed). `sha256` is the hash of the assembled body: the name's `<h>` for `packs/<h>.pack` and `units/<h>`, the same as `x-blaze-sha256` for `.pack.zst` |
| `DELETE <base>/<name>?upload=<id>` | aborts a multipart upload: 204 |
| anything else (DELETE of an object, listing) | 405 |

Every `PUT` carries `Content-Length` (411 without it): R2 takes a body of known length, and the
size is checked before the body is read. The `ETag` header is quoted, the JSON `etag` bare;
`If-Match` takes either. An `<id>` from `?uploads` is opaque and URL-safe; it binds the upload to
its tenant and name. A completed multipart upload is read back through the Worker into its name
(the binding has no copy), so it costs one more read and write than its parts.

Errors carry a one-line plain-text reason. 429 with `Retry-After` from the per-tenant (or per-IP
for anonymous) rate limit. Every request writes one Analytics Engine data point (tenant, op,
status, bytes in, bytes out).
