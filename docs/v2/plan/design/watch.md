---
status: plan
summary: "The monitor's plumbing (bottom.md section 14): per-thread wait-free seqlock rings in every blaze process, the flight recorder over the same rings, a framed transport with credit flow control and resume by sequence number, and the viewer helper's core (live runs from the registry, pages on ix.dev over loopback HTTP/1.1 and server-sent events, pairing by a code a person compares, one permitted action, an opt-in relay uplink), with a fixed expiry. Built as sans-IO machines with simulations in crates/blaze-watch; not built: the drivers."
date: 2026-11-15
---
# Watch: from a thread's event to a browser

What a running `blaze` command does reaches a person's browser, and a stall leaves a recording.
Everything here is a sans-IO machine in `crates/blaze-watch` (no third-party crate, `no_std`,
allocator passed in, no `unsafe`); the drivers are not written. Parts are adapted from
makepad/relax (MIT, `licenses/relax-MIT.txt`), each place marked in the code.

## The picture

```text
app thread --push (11 atomic ops)--> its ring --drain--> Feed (window, by seq)
                                                            |  Unix socket, 0600, frames
                                                            v
 ix.dev page <--loopback HTTP/1.1 + SSE--  Viewer  <--------+  (one link a live run)
                                             |
                                             +--opt-in, dialled out--> relay on ix.dev
```

A run's drain thread empties its rings into its `Feed`; the viewer follows every run the
blaze-run `Registry` lists, merges the streams into one bounded window, and serves pages. No
part pushes into a slower one: each reader pulls by sequence number under credit, so a slow
reader costs a cursor, never memory, and learns what it missed as a gap.

## The machines

**Emitter** (`ring`): a fixed table of single-producer rings, one a thread, made once
(`lanes * (len + 1) * 64` bytes). An entry is one 64-byte line: a stamp and seven payload words
(kind, flags, detail, time, value, a 32-byte key). The protocol is Boehm's seqlock with every
word atomic, so there is no data race and no `unsafe`. A push is always the same eleven steps:
- the stamp made odd (`Relaxed`);
- a `Release` fence, so a reader that sees any payload word and then fences `Acquire` sees the
  odd stamp on its second load;
- seven payload stores (`Relaxed`);
- the even stamp `(n+1)<<1` (`Release`), so an `Acquire` load of it sees the whole payload;
- the head `n+1` (`Release`), so a reader that loads it `Acquire` knows every entry below it is
  stamped.

There is no read-modify-write, no lock and no wait: an app thread is never held, which is the
128-core rule. When a ring is full the oldest entry is overwritten. A drain is a step machine, one
atomic operation a step:
- the head (`Acquire`);
- for each entry: its stamp (`Acquire`), the payload (`Relaxed`), an `Acquire` fence, the stamp
  again;
- an entry is kept only when both stamps are its own.

Lapped and torn entries come back as `Lost{from, count}`, by number, so delivered plus lost is
exactly the number pushed. Lanes are claimed at thread start by CAS (the emitter's only RMW); a
dropped `Producer` marks its lane ended, and the drain frees it after its last drain.

**Recorder** (`recorder`): it watches `WAIT_BEGAN` and `WAIT_ENDED` as they are drained:
- a wait open 150 ms samples its thread's stack;
- a wait over 1 s (a hitch), one still open past it (a hang), a panic, or a person asking saves
  a recording of the last 60 s of every ring;
- automatic saves come at most every 30 s, and 12 are kept, oldest removed first.

The flight recorder is the same rings, read from `head - len` without moving any cursor.

**Recording file** (`pack`): `BWR`, a version byte, then blocks. A block is:
- its row count and a key table (each distinct key once);
- eight columns (kind, flags, detail, lane, n, time, value, key index), each a byte length
  followed by LEB128 values, with n, time and value as zigzag deltas.

The reader refuses a non-shortest number, a column that does not hold its rows, a key index past
the table, more than 65,536 rows or 4,096 keys.

**Conn** (`conn`): one end of a connection. It holds:
- per channel, the `Credit` it may send and the `Grant` it owes;
- the control frames it owes (at most 64);
- the order channels are written in (by priority, then channel).

Each data channel starts with 256 KiB of credit. The receiver grants back what its consumer took,
once 64 KiB has built up. An overgrant, overspend or overrun is a typed refusal and closes the
link. Control frames are never held by credit.

**Feed and Follow** (`feed`): the run's side and the reader's side of a stream.
- `Feed` holds a `Window`: a fixed byte ring and an index, numbered from 1, dropping the oldest.
- A `Subscriber` per connection pulls DATA while credit lasts, and sends one GAP for anything the
  window dropped first.
- `Follow` takes items strictly in order: a repeat is `Replayed`, a hole with no gap is
  `Skipped`. It keeps the peer's epoch. Resume is `OPEN{from: last}` for the same epoch and
  `from: 0` for a new process.

**Viewer** (`viewer`): a step machine whose events are ticks, registry listings, run frames,
page bytes and writes, and local commands. Its effects (connect, bytes to a run, page or command,
close, store the keys file, exit) carry spans of one output buffer.
- Runs live in a `Soa`, at most 256. A link is Connecting, then Open, then Lost; it is
  reconnected at the next listing that still names the run.
- The pages' window is 4 MiB and 65,536 items.
- There are at most 16 pages. Each has one request (at most 24 KiB, due within 10 s) and an
  unwritten queue of at most 64 KiB plus one event.
- A quiet stream gets a keepalive comment every 15 s.

**Uplink** (`relay`): the viewer dials out and sends HELLO with its epoch. The relay opens the
events channel from its last item, and the uplink streams the window under the relay's credit.

## The wire

**Frames** (run socket and relay): a `u32` LE length (at most 80 KiB), then one deterministic
CBOR array, `[1, chan, kind, ...]`. CBOR here means unsigned ints, byte and text strings, arrays,
booleans, definite lengths and shortest heads only, with no maps (so no key order). Channel 0 is
control and channel 1 is events; there are at most 16 channels.

| kind | fields | limits |
|---|---|---|
| 0 HELLO | role (run, viewer, relay), epoch | control only |
| 1 OPEN | service (0 events), from | |
| 2 DATA | seq (from 1), bytes | at most 64 KiB |
| 3 GAP | from, to | `1 <= from <= to` |
| 4 CREDIT | bytes | `u32`; overgrant refused |
| 5 CLOSE | code (normal, protocol, refused) | |
| 6/7 PING/PONG | time | |
| 8 REQUEST | bytes: blaze-run's `[1, 0, hooks, on, [slot...]]` | at most 64 KiB; viewer to run only |

**An item** (DATA on events) is `[0, lane, n, kind, flags, detail, at, value, key]`, with the key
0 or 32 bytes (empty means zero), or `[1, lane, from, count]` for lost entries.

**A page item** is base64 CBOR in an SSE event:
- `[1, 0, pid, start, seq, item]`;
- `[1, 1, pid, start, checkout, program, began]` (listed);
- `[1, 2, pid, start]` (gone);
- `[1, 3, pid, start, from, to]` (the run's gap).

SSE ids are `epoch-seq`. The first event is `hello`: `[1, epoch, first, newest]`. A gap event is
`[1, from, to]`.

**HTTP** limits:
- a head of at most 8 KiB, lines of at most 2 KiB, at most 32 headers (431);
- a body of at most 16 KiB by `Content-Length` (413);
- refused with 400: `Transfer-Encoding`, folded lines, non-visible bytes, a repeated Host, Origin,
  Authorization or Content-Length, and any bytes after the request;
- methods other than GET, POST and OPTIONS get 405; versions other than 1.1 get 505.

One request per connection, `Connection: close`. History asks at most 256 rows; a toggle names at
most 4,096 slots.

## Security

- **The local socket** is in the user's 0700 directory, mode 0600. The driver checks the peer's
  uid (`getpeereid` or `SO_PEERCRED`), and the machine refuses any uid but its own. The same holds
  for each run's socket.
- **The viewer's port** (47823) is bound to 127.0.0.1 only.
- **`Host`** must be `127.0.0.1`, `localhost` or `[::1]` with the port, else 421. This defeats DNS
  rebinding.
- **Origin.** Only `https://ix.dev` gets past the static script.
  - Any other origin gets 403 with no CORS header, so it cannot read even the status.
  - Preflights get `Access-Control-Allow-Origin` for the one origin, `Vary: Origin`, the methods
    and headers, and `Access-Control-Allow-Private-Network: true` when asked (Chrome's
    loopback-from-public rule).
  - Every refusal has an empty body, and its bytes do not depend on the viewer's state (a
    simulation compares two viewers).
- **Pairing**:
  1. The page POSTs `/v1/pair` and gets a 128-bit ticket and an 8-character Crockford code
     (40 bits), which it shows.
  2. The next blaze command asks the viewer whether a pairing waits, prints the code and asks
     the person whether the browser shows the same. A yes confirms it.
  3. The page polls with its ticket and is given a 256-bit key once.

  The page never submits a code, so guessing gains nothing. One pairing waits at a time, for at
  most 120 s. At most 16 browsers are kept. The keys file is written whole, 0600, by rename, and
  keys are compared in constant time. A key's `Debug` prints nothing.
- **Reads versus actions.** `/v1/runs`, `/v1/events` and `/v1/history` read; `/v1/toggle` is the
  only action. All of them need `Authorization: Bearer <key>`, else 401. The toggle is forwarded
  only to a listed run whose link is open, as blaze-run's own request.
- **The relay** carries the activity a paired page sees, with item keys zeroed unless the
  organisation chose to carry names (bottom.md section 14: activity only, not code or item names,
  unless chosen). It never carries pairing keys or codes, history, argument values (no event holds
  them) or any action. A REQUEST from the relay is refused and closes the uplink.

## Expiry (rule 70)

The viewer holds nothing a record cannot rebuild: the window refills from the runs' windows, and
the keys file is on disk. Let `t` be the moment the registry last became empty. The viewer exits
at the first tick when either:
- no paired stream is open, `t + 30 s` has passed, and 5 s have passed since the last stream
  closed; or
- `t + 10 min` has passed, whatever is open.

Why these numbers:
- 30 s spans the gap between one command and the next in an edit loop, so a person's page does
  not drop between builds.
- 5 s lets a reloading page reconnect.
- 10 min bounds a forgotten tab: rule 70 allows a helper with an expiry, not one a page keeps
  alive.

While a run is live there is no expiry: the runs are what it serves. The longest the viewer
outlives the last run is therefore 10 min plus a tick. The stream bound is the page's 64 KiB queue
plus one event. The simulation restates the rule independently and checks every tick against it.

## Where the page's script comes from

There are two choices:
- **ix.dev serves it.** That needs a deploy per change, its version can drift from the local
  viewer's schema, and the viewer cannot enforce a pinned digest on a script it never sees.
- **The blaze binary carries it** and the viewer serves it at `GET /v1/app.js`: static bytes,
  `Cross-Origin-Resource-Policy: cross-origin`, no auth. Its version is the viewer's schema, it
  works offline, and it is reviewed with the code that speaks to it.

**Chosen: the binary.** The ix.dev page is a thin loader. The residual trust is in the ix.dev
origin itself, which could load other code. That is the same trust pairing already places in it.

## What other crates must provide

- **blaze-run**:
  - serve the event feed on its socket as these frames (replacing `<name>.events`);
  - take its `Toggle` as a REQUEST frame;
  - have its threads emit into `ring::Emitter`;
  - move its 88-byte `Entry` and `Toggle` wire forms into one zero-dependency home both crates
    take (`run_wire` restates them, held by a layout test).
- **CBOR**: one zero-dependency home. `cbor` copies blaze-pack's, because blaze-pack links serde
  through blaze-report (rule 30).
- **bstd / the driver**:
  - a Unix socket created 0600 in a 0700 directory, with a peer uid check;
  - a loopback TCP listener;
  - deadline waits;
  - OS random bytes;
  - a stack sampler for a thread by lane;
  - a timer;
  - a whole-file write by rename;
  - the `Cx` slot that holds a thread's `Producer` (there is no thread-local storage).
- **licenses/NOTICE**: a line for Relax.

## Proven, and not

The simulations, each with a planted-bug control:
- N producers interleaved at every atomic operation never wait, never deliver a torn entry, and
  lose exactly the oldest by count;
- a resumed feed over a dropping link delivers each item once or a gap;
- a slow page stays within its queue;
- refused requests get their status and learn nothing;
- only a paired page toggles;
- the expiry rule holds;
- recordings round-trip.

The ring simulation's cells are sequentially consistent: it proves the protocol's logic, not its
fences. The orderings above are the claim blaze-check is to verify under weak memory.

Open: whether Safari lets an https page fetch `http://127.0.0.1`. Chrome's Private Network Access
prompt is answered, but Safari may need the relay even for a local page.
