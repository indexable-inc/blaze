---
status: plan
summary: Lane speed-link-edge-ci's notes for blaze v2: the CI service, its GitHub path and its UIs made cheap per event, with counts before and after and what each is measured against.
date: 2026-10-29
---
# blaze v2: speed-link-edge-ci's notes

Notes on making `crates/blaze-ci`, `crates/blaze-ui` and `crates/blaze-web` cost little per event,
2026-10-29. The design they speed up is in [notes.md](notes.md). `blaze-ui` and `blaze-web` were
done by two children; their notes are [speed-link-edge-ci-ui.md](speed-link-edge-ci-ui.md) and
[speed-link-edge-ci-web.md](speed-link-edge-ci-web.md).

The numbers below are counted from the code, per event, in steady state (a token already minted,
a connection already open). None is measured yet: this lane runs no builds or benchmarks. The
benchmarks that will measure them are listed at the end.

## Webhooks (`webhook.rs`)

- **One borrowed decode.** A pull request payload was decoded twice (`ActionPayload` for the
  action, then the whole `PullRequestPayload`), each into owned `String`s. Now one
  `PullRequestPayload<'a>` decodes once, its strings `Cow<'a, str>` borrowed from the body
  unless JSON escapes them; unknown fields are skipped by serde without building a
  `serde_json::Value`. An action blaze runs on that lacks `number`, `pull_request` or
  `repository` is a 400 naming the field, as before.
- **Signature on the stack.** The `X-Hub-Signature-256` hex decodes into a `[u8; 32]` and is
  checked with `ring::hmac::verify` (constant time); a malformed one is a 401.

| per webhook | before | after |
|---|---|---|
| JSON decodes of the body | 2 | 1 |
| string copies while decoding | each field, twice for the repository | 0 (escaped strings only) |
| copies kept in the `Event` | the shas, refs, repository | the same: the service keeps them |
| allocations for the signature | 1 (`Vec<u8>`) | 0 |

Left as is: `listen.rs`'s `Seen` dedupe allocates the delivery id twice (set and queue). That
is two small allocations per webhook, at most about one a second.

## The GitHub API (`github.rs`)

- **The body is written, not built.** A check run's JSON was a `json!` tree (a map, its nested
  `output` map and a `String` per value, the summary cloned up to 64 KiB) serialized into a new
  `Vec`. `check_run` now writes the keys in serde_json's sorted order straight into a buffer
  the client keeps, escaping as serde_json does (lowercase `\u00xx`); a word-at-a-time test
  finds the bytes that need escaping, so a summary with none is one copy.
- **The request is written into a kept buffer**, in the same header order and bytes as
  `blaze_api::http::request` (tested against it), on the one TLS connection the client keeps.
- **The token is borrowed.** `fresh()` lends the current token; it was cloned per request.
- **Superseded updates are dropped.** `runtime::post` drains every post waiting and skips an
  update when a later update of the same run is waiting, so a burst of changes to a run is one
  request.

| per check-run update | before | after |
|---|---|---|
| allocations for the body | about 10 (tree nodes, strings) + the output `Vec` | 0 (kept buffer) |
| copies of the summary | 3 (into the tree, cut, serialized) | 1 (into the body) |
| allocations for the request head | 1 `Vec` + `format!`s | 0 (kept buffer) |
| token clones | 1 | 0 |
| requests for `n` changes made before the first is sent | `n` | 1 |

Asked of the wire lane (`blaze-api`, see the report): an `http::request_into(&mut Vec<u8>, ..)`
so the copy here can go, and response headers and body that borrow instead of allocating per
response.

## The state machines (`service.rs`, `runtime.rs`)

- **Ids, not clones.** A job's runs were a `Vec<RunId>` cloned before every walk; they are now
  the range `first..first + count` (ids are given out together). A run's work was a clone of its
  whole `Check` (name, arguments); it is now `Work`, 8 bytes, `Copy`: the check's kind and its
  index in the settings. A run kept `posted`, a clone of its `CheckRun` made on each update to
  compare against; it is now an `unposted` flag set only while GitHub has not answered the
  creation.
- **A per-run `pending` flag and a per-job count** in place of a `BTreeSet<RunId>` per job.
- **Hash tables on ids** (`FxHashMap`) for jobs and runs; no map in the service is iterated, so
  none decides an order.
- **One save per burst.** `Output::Save` carried a clone of the whole state (every checkout and
  snapshot) and the runtime wrote and fsynced it each time. It is now a unit variant; the serve
  loop steps up to 1024 inputs waiting and writes the state once after them.
- **No state change, no update.** `set` returns before touching anything when the status and
  summary are what they were.
- **Summaries** are written with `write!` into one `String` sized from what they list, not a
  `format!` per line; a failed test's output tail reads from the end of the output (doubling
  until it has its 30 lines) instead of splitting all of it into lines, so a test that printed
  megabytes costs what its last lines do. It is tested against the old whole-output tail.

| per event | before | after |
|---|---|---|
| clones of a job's run list | 1 per walk (replace, finish, checkout) | 0 |
| clones of a `Check` per run | 1 | 0 |
| clones of a `CheckRun` per update | 2 (`posted`, the output) | 1 (the output) |
| clones of the whole state per save | 1 | 0 |
| state writes (fsync) for `n` saves in a burst | `n` | 1 |
| lookups | `BTreeMap` (`log n` compares) | one hash of a `u64` |

## Checkouts (`checkout.rs`, `runtime/worker.rs`)

- **No fetch for a commit the clone has.** `Git::has` (`rev-parse --verify --quiet
  <c>^{commit}`) runs before anything else; only a missing commit costs an installation token
  and a fetch.
- **No worktrees at all.** A checkout diffs git objects (`diff-tree`, then `cat-file --batch`
  for the changed blobs) against the tree it starts from and sends only those as ops, so there
  is no worktree to reuse: this is cheaper than reusing one.
- **Git's output is parsed in place.** `diff`, `listing` and `batch` lines split into fixed
  arrays of `&str` borrowed from git's stdout; they were a `Vec<&str>` and a `String` per
  field.
- **Ops are moved into batches.** `batches` takes the ops and moves each into its batch; a
  large file keeps its buffer for the first piece and copies out only the rest. They were
  cloned. `apply` moves each batch into its request unless it must be sent twice (the default
  branch commits the same ops to its head).

| per checkout of `n` changed files | before | after |
|---|---|---|
| fetches when the commit is there | 1 (+ a token mint) | 0 |
| copies of each changed file's bytes | 2 (the batch, the request) | 1 (the request; 2 on the default branch) |
| allocations per changed path parsed | a `Vec` + a `String` per field | 0 |

Asked of the wire lane: `Frames::next_ref` and `fill` public, so the worker decodes daemon
replies in place, and `Request::Move` (move a head from one commit to another) so the default
branch's ops are not sent a second time.

## `blaze-ui` and `blaze-web`

The details are in the children's notes. These are their summaries.

**`blaze-web`** ([speed-link-edge-ci-web.md](speed-link-edge-ci-web.md)): `Graphs::record` takes
no lock, and records are encoded once and kept compressed in one stream per build. Pages get
frames of up to 4 096 records written in one syscall, and the page uploads to the GPU once per
animation frame. The page still decodes each record into an object: columns would need a WASM
SIMD decoder, which is left as future work.

| per record | before | after |
|---|---|---|
| locks on the daemon's side | 1 | 0 |
| encodings | 2 | 1 |
| websocket messages per page | 1 | 1 per batch of up to 4 096 |
| `write` syscalls per page | 2 per message | 1 per batch |
| GPU uploads on the page | 1 per `Functions` record | 1 per animation frame |

**`blaze-ui`** ([speed-link-edge-ci-ui.md](speed-link-edge-ci-ui.md)): indexes in `Build` in
place of linear scans per record. The live region is rendered into one reused buffer and only
the lines that differ from the last drawn region are written. Plain output goes through one
buffer, with no `String` per line.

## Unsafe ([unsafe.md](../../design/unsafe.md))

None of the speed above needed `unsafe`. `blaze-ci` and `blaze-ui` say `#![forbid(unsafe_code)]`.
In `blaze-web`, the only unsafe module is `flecs.rs`: it holds the Flecs C calls, behind a safe
API, with a `// SAFETY:` comment on each block, and every other module forbids unsafe code.

For the safety lane:

- **Fuzz targets** for the parsers of untrusted bytes:
  - the webhook body and signature (`webhook::parse`);
  - GitHub's API responses (`github.rs`);
  - git's `diff-tree`, `ls-tree` and `cat-file --batch` output (`checkout.rs`);
  - the websocket frame reader and HTTP request parser (`blaze-web/src/http.rs`).
- **Model checking**:
  - `blaze-web`'s `Roots`: safe code, but its reads are lock-free (acquire loads of `len`
    and `OnceLock` slots, with writers under a mutex);
  - the record channel to the page's thread.

## What measures them

- **JSON**: simdjson and sonic-rs for decoding a webhook body; the borrowed serde decode
  should be within a small factor of them at webhook sizes (a few KiB to 100 KiB).
- **CI runners**: Buildkite's agent and GitHub Actions runners, for the time from a webhook
  to the first check run update and from a push to its result.
- **Terminal rendering**: ratatui's diffed buffer, for bytes written and allocations per frame.
- **Web rendering**: a renderer without a virtual DOM (keyed, direct DOM writes), for the time
  from a record arriving on the websocket to its pixel.
