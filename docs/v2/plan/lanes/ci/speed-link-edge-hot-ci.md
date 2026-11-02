---
status: plan
summary: blaze v2 ci lane, hot_lint pass on blaze-ci: the webhook, service step and worker read loop allocate only what they hand on, each held by a budget test; Clone off the big types; lint 308 to 0.
date: 2026-10-29
---
# speed-link-edge-hot-ci: allocation on blaze-ci's hot paths

Part of [ci](../ci.md), after [speed-link-edge-ci](speed-link-edge-ci.md). Path: `crates/blaze-ci/`.
Nothing here was built or run (docs/v2/plan/goals/principles.md). The counts below are read off
the code, and the budget tests listed hold them once they run.

## The hot paths

Most of blaze-ci is the edge: GitHub's HTTP API, the config file, git checkouts, snapshots and
the standby. Each of those runs once per start, once per job or once per check-run post, and
costs a process, a TLS round trip or an fsync. Three paths run per input:

- **Webhook to queued event**, in `runtime/listen.rs` and `webhook.rs`: a request read, its
  signature checked, the payload decoded, the delivery deduplicated and the event sent to the
  service.
- **Service step**, in `service.rs`: one `Input` (mostly a daemon `Response` of a running job)
  stepped into outputs. A test run sends one response per test, so this is the busiest path.
- **Worker read loop**, in `runtime/worker.rs`: the daemon's frames cut and decoded, then handed
  to the service, plus each request encoded and written.

## State of the art

- **GitHub webhook receivers (octokit/webhooks, probot):** these parse the whole payload into
  owned objects, then dedupe on `X-GitHub-Delivery` with an unbounded set or an external store.
  blaze-ci borrows the payload's strings from the request (sonic-rs `Cow` fields). It checks the
  repo and dedupes before anything is owned, and keeps the last 10 000 delivery ids in a fixed
  ring whose strings are reused.
- **HMAC verification (ring, `hmac` crate):** the key schedule is computed once per secret, and
  each delivery costs one `verify`. `Verifier` does this; before, the key was set up per
  delivery.
- **Line-oriented aggregators (cargo's and nextest's reporters):** these append each finished
  test or diagnostic to one growing buffer rather than a `Vec<String>`. `Report` now does the
  same, measuring each item first so the buffer grows at most once per item.
- **Length-delimited framed readers (tokio-util, hyper):** these keep one buffer per connection
  and decode in place. The worker now reads through blaze-api's `Frames::next_ref`, and writes
  through one kept `cbor::Writer` and frame buffer.

## Before and after, per steady-state input

| path | before | after | test |
|---|---|---|---|
| a delivery checked and read (`receive_ref`) | an owned request, HMAC key set up, every payload field owned | 0 | `webhook/tests.rs` `a_warm_delivery_allocates_only_its_events_strings` |
| the event made owned (`EventRef::into_owned`) | (part of the line above) | 2 for a push, tag or pull request (its strings), 0 for one ignored | as above |
| a new delivery over a warm connection (`exchange`) | the above, plus the id twice (set and queue), the answer's `format!`, the response `Vec` | at most 3: the event's 2 strings and at most one block of the service's channel (std's mpsc allocates one per 31 sends) | `runtime/tests.rs` `a_warm_webhook_delivery_allocates_only_the_event_it_hands_on` |
| a redelivery, a ping | the id and answer formatted, a response `Vec` | 0 | as above |
| the service steps a passed, ignored or unreached test, or a warning | a `String` per result kept in a `Vec<String>` | 0 | `service/tests.rs` `a_warm_service_step_allocates_only_what_it_posts` |
| the service steps a listed error | a rendered `String` pushed to a `Vec` | at most 1 (its list's buffer growing), 0 past `MOST_LISTED` | as above |
| a green run's end | the summary joined from the `Vec`s, every list cloned | 4: the summary and the check run's 3 strings handed to the GitHub lane | as above |
| a repeated end, a late creation | clones of the run's state | 0 | as above |
| the worker reads a `Done` | 0, read in place already (speed-link-edge-ci) | 0, now held by a test | `runtime/tests.rs` `a_workers_warm_reads_allocate_only_what_each_response_owns` |
| the worker reads a `TestResult` | its 3 fields | 3: its binary, name and output, handed to the service | as above |
| the worker sends a request (`Outgoing::send`) | a fresh frame `Vec` per request (`frames::encode`), and the socket's name formatted | 0 | `runtime/tests.rs` `a_workers_warm_requests_are_written_from_one_buffer` |

Each budget is measured warm. The parser, the `Answer`, the service and `Outgoing` have each
served once before the measured call.

## What changed

- `webhook.rs`: adds `DeliveryRef` and `EventRef` (lent from the request), and `Verifier` (the
  HMAC key, made once). `receive_ref` now takes `&Verifier` and returns `DeliveryRef` (the
  callers are `listen.rs` and the tests). `parse_ref` is new. `receive`, `parse` and `verify`
  keep their signatures and their bytes.
- `runtime/listen.rs`: `Seen` is a FIFO ring of reused id strings, plus a
  `hashbrown::HashTable<u32>` sized at twice the ring. It rehashes in place and never grows. A
  model test checks it against the old `VecDeque` + `HashSet`. Each handler keeps one parser
  and one `Answer`, and answers through `http::response_into` (from
  [speed-link-edge-hot-wire](speed-link-edge-hot-wire.md)).
- `service.rs`: `Report` keeps each list as one `String` and is checked over seeded rounds
  against the old `Vec<String>` layout. `tail_into` and `render_*_into` write in place. `keep`
  borrows its ref, and `unpinned` makes one `retain` pass.
- `runtime/worker.rs`: `Outgoing` holds a kept `cbor::Writer` and frame. `drain` reads frames in
  place. A lost or cancelled run's ids are sorted with `blaze_radix::sort_copy_by_key`.
- The edge files (`runtime.rs`, `checkout.rs`, `github.rs`, `config.rs`, `snapshot.rs`,
  `runtime/pins.rs`, `standby.rs`, `oracle.rs`, the binary) are justified line by line: once at
  start, once per job, once per checkout, once per snapshot, once per token, or an error path.
  Three fixes made errors lazy or wrote in place: checkout's `git failed` error, github's
  `connect` error, and the oracle's summary. `config.rs`'s tests moved to `config/tests.rs`.

## Clone

`Clone` was removed from types nothing clones: `Event`, `Train`, `Delivery`, `WebhookError`,
`Kept`, `SnapshotError`, `CheckoutError`, `GitHubError`, `ConfigError`, `Secret`, `Config` and
its `App`/`Webhook`/`Daemon`/`Git`/`Oracle`/`Standby` parts, standby's `Input`, `Action` and
`Standby`, and service's `Settings`, `State`, `Held`, `Input`, `Output`, `JobKind`, `Job`, `Run`
and `Report`. It was kept, with a `// clone:` line saying who clones it, on `CheckRun` (once per
check-run post), `Checkout` (once per checkout), `Git` (once per checkout, out of its lock),
`Token` (once per token per lane), `Check` (once, at start), `TargetsConfig` (once per sampled
view) and `SnapshotConfig` (once per snapshot).

## Lint (`/tmp/sle_lint.py`)

| file | before | after |
|---|---|---|
| `webhook.rs` | 10 | 0 |
| `runtime/listen.rs` | 10 | 0 |
| `service.rs` | 53 | 0 |
| `runtime/worker.rs` | 39 | 0 |
| `runtime.rs` | 48 | 0 |
| `checkout.rs` | 41 | 0 |
| `github.rs` | 34 | 0 |
| `config.rs` | 30 | 0 |
| `snapshot.rs` | 14 | 0 |
| `runtime/pins.rs` | 13 | 0 |
| `standby.rs` | 11 | 0 |
| `oracle.rs` | 4 | 0 |
| `bin/blaze-ci.rs` | 1 | 0 |
| **blaze-ci** | **308** | **0** |

## Open until it runs

- Whether sonic-rs allocates inside a borrowed `Cow` decode, for payloads with no escapes. The
  webhook budget of 0 says it does not; if it does, the fix is in the decode, not the budget.
- Whether std's mpsc channel allocates a block on the measured send: the budget allows one.
