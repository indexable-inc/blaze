---
status: plan
summary: Index of speed-link-edge's work on the ci lane's crates (blaze-protocol, blaze-api, blaze-ci, blaze-ui, blaze-web), with where each part's before/after is.
date: 2026-10-29
---
# speed-edge: the agent API, CI and the build views

Part of [ci](../ci.md).

| part | code | notes |
|---|---|---|
| CBOR codec, frames, API server and client | `crates/blaze-protocol/`, `crates/blaze-api/` | [speed-link-edge-wire](speed-link-edge-wire.md) |
| CI service, webhooks, GitHub | `crates/blaze-ci/` | [speed-link-edge-ci](speed-link-edge-ci.md) |
| terminal view | `crates/blaze-ui/` | [speed-link-edge-ci-ui](speed-link-edge-ci-ui.md) |
| build page | `crates/blaze-web/` | [speed-link-edge-ci-web](speed-link-edge-ci-web.md) |

## In one line each

- **Wire**: the encoder now writes in place and permutes a map only when its keys come out of
  canonical order. The decoder is one pass that checks determinism as it goes. Frames are written
  vectored and read into a reused buffer. The old codec is kept as the byte-for-byte oracle
  (`cbor::reference`). The bytes are unchanged.
- **CI**: each webhook is decoded once with borrowed fields. Check-run JSON and HTTP requests are
  written into kept buffers, and superseded updates are dropped before sending. The worker decodes
  the daemon's frames in place (`Frames::fill`/`next_ref`), with no `Vec` per frame.
- **UI**: the region is rendered into one reused frame and diffed line by line against what is on
  screen, rather than cleared and redrawn.
- **Web**: recording a build takes no lock. Records are encoded once, straight into the program's
  buffer (`cbor::append`), and every page shares one frame of many records.

Still open, and outside speed's remit: `HttpResponse` borrowing from the read buffer,
`Request::Move` (a compare-and-swap of a head) and CBOR byte strings for `Vec<u8>` fields. The
last needs a `SCHEMA` bump because the bytes change.
