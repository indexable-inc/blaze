---
status: current
summary: Reaching the daemon with no process launch: blaze-client from Rust, its C ABI, the Node-API addon for TypeScript, and the small `blaze-fast` command line.
---
# Calling the daemon without launching blaze

`blaze` links the compiler, so starting it costs milliseconds before it asks the daemon
anything. A caller that holds a connection pays one socket round trip a request instead. Each
of the clients below is a client of the agent API (`crates/blaze-api/src/lib.rs`: one versioned
schema, deterministic CBOR, the daemon's Unix socket); none adds a message or a second
protocol.

| Caller | What | Where |
|---|---|---|
| Rust | `blaze_client::Client` | `crates/blaze-client` |
| C | `blaze_client_connect`, `_request`, `_call`, `_close` | `crates/blaze-client-c`, `crates/blaze-client-c/include/blaze_client.h` |
| TypeScript (Bun, Node 20+) | `connect()`, `Client` | `crates/blaze-napi` (`index.js`, `index.d.ts`) |
| A shell | `blaze-fast` | `crates/blaze-client/src/bin/blaze-fast.rs` |

## How the daemon is found and started

The socket is the CLI's: `v2.sock` in `$BLAZE_HOME`, else in `$XDG_RUNTIME_DIR/blaze`, else in
`~/.cache/blaze`. A connect names a home to use in place of `BLAZE_HOME`. A warm client only
connects.

When nothing listens: where `blaze daemon install` has launchd or systemd serve the socket,
the connect itself starts the daemon, and a unit that serves it with nothing listening is an
error naming the unit. Otherwise the client runs `blaze serve --ready-fd N` once, as the CLI
does (its own process group, output appended to `serve.log` in the state directory), and
waits until the daemon says it listens. The `blaze` program is the one the connect names, else
`blaze` beside the calling program, else the first on `PATH`; when there is none the connect
stops, naming where it looked. Nothing else is tried.

A client is of no build of blaze, so it makes neither of the CLI's checks of the daemon it
reaches (same build, same settings). A daemon of another schema is refused on its first
answer, naming both schemas.

## Deadlines and errors

Every call takes how long it may take (`within`, `within_ms`). A request past its deadline
ends with a typed error and is cancelled at the daemon; the connection goes on. Errors carry
a code: 1 another schema, 2 bytes refused, 3 a response to no request, 4 the daemon hung up,
5 the deadline passed, 6 a read or write failed, 7 the client was closed, 8 the daemon
answered with an error, 9 the connect failed, 10 no daemon and none to be started, 11 the
daemon could not be started, 12 what was asked is no request, 13 a panic inside a foreign
binding, 14 a null or non-UTF-8 argument (C).

## Rust

```rust
use std::time::Duration;
use blaze_client::{Client, Options};

let mut client = Client::connect(None, Duration::from_secs(60))?;
let status = client.status(Duration::from_secs(10))?;
let built = client.build(&Options::new("/path/to/crate", 600_000), |response| {
    // each response as it comes: units, diagnostics, what the build is doing
})?;
for diagnostic in &built.diagnostics {
    eprint!("{}", diagnostic.rendered);
}
```

`build`, `check` and `test` take an `Options` (the fields of the API's `Targets` and `Cargo`)
and give an `Outcome`: the request's `Done`, the diagnostics, the artifacts, the test results,
the errors. `why` gives the API's `Rebuilt`, `status` its `Status`. `request` sends any
`Request` whole. A command is sent as the CLI sends it (its cargo command line, the
environment, cargo's config files outside the checkout), so the daemon builds what `blaze` in
that directory would. `test` runs the tests in the daemon (`Request::Test`); `blaze test` runs
the test executables itself.

For many requests at once, `send` gives an id and `next` the next event of any of them. The
core under it (`blaze_client::Mux`) is sans-IO, for a caller with its own event loop.

## C

Link `libblaze_client_c` (a `cdylib` and a `staticlib`) and include `blaze_client.h`.

```c
blaze_error error = {0};
blaze_client *client = blaze_client_connect(NULL, 60000, &error);
const char *options = "{\"cwd\":\"/path/to/crate\",\"within_ms\":600000}";
char *answer = blaze_client_call(client, "build", (const uint8_t *)options, strlen(options),
                                 on_event, NULL, &error);
blaze_string_free(answer);
blaze_client_close(client);
```

`blaze_client_call` takes a method (`build`, `check`, `test`, `why`, `status`, `request`) and
its options as JSON, and returns the answer as JSON. `blaze_client_request` sends one
`Request` of the API as its JSON or its CBOR. Each response goes to the callback, on the
calling thread, as one line of the API's JSON (or the envelope's CBOR). The header states
every pointer's rule. No call unwinds into the caller.

## TypeScript

```ts
import { connect } from './crates/blaze-napi/index.js';

const client = await connect({ within_ms: 60_000 });
const built = await client.build({ cwd: '/path/to/crate', within_ms: 600_000 }, (response) => {
  if ('Diagnostic' in response) process.stderr.write(response.Diagnostic.rendered);
});
await client.close();
```

The addon is the one binding. Each client has a worker thread that owns the socket; a call
returns a promise settled from that thread, and any number may be under way at once. A client
with no call under way does not keep the process alive.

To build it: build the package `blaze-napi` with blaze, then copy its library
(`libblaze_napi.dylib` on macOS, `libblaze_napi.so` on Linux) to `blaze.node` in `crates/blaze-napi`,
or name the file in `BLAZE_NAPI`. The smoke test connects, asks the status, builds a crate it
writes, times a warm request and closes:

```sh
bun crates/blaze-napi/test/smoke.ts
```

## blaze-fast

`blaze-fast build|check|test|why|status` asks the daemon through the client library. It reads
the options `blaze-fast --help` lists (packages, features, profile, target, the target
selection flags, `--color`, `-v`, `-q`, `--json`) and refuses any other word by name, saying
to use `blaze`; it never runs `blaze` in its place. It prints each diagnostic as the daemon
rendered it, blaze's reports through blaze-report's renderer, and cargo's `Compiling`,
`Checking` and `Finished` lines; with `--json`, each response as one line of the API's JSON.
`why` and `status` print the answer as JSON. It exits 0 when the request ended ok, 101 when it
did not, 2 for a command line it does not handle.
