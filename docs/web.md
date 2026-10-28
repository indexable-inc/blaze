# The build page

`blaze ui` prints the address of a page the daemon serves on 127.0.0.1 and opens it: the build
as it goes, down to every function. Nothing of it runs until it is asked for: the first
`blaze ui` starts its server in the daemon, which serves it until the daemon exits.

## What it shows

- **The program's functions**, every one, drawn on the GPU: each function the build put in the
  program is a point, each call a line. Colour is the crate, points cluster by crate and then
  module, and a point's size is its compile time. As a build goes, a function lights up when
  it is compiled; one taken from the store or unchanged since the last build is dim. After an
  edit, the functions it compiled again light up from the edited one outward.
- **The crate DAG**: the crates and what each calls into, a crate opening to its modules and a
  module to its functions.
- **Search**: `crate::path::fn`, with its callers and callees a few calls out.
- **Compile time**: a treemap of crate > module > function.
- **Phases**: the build's steps on a timeline.
- **The edit**: from the edited file to every function compiled again, and why (`blaze why`,
  drawn).

## Where it comes from

The page reads the records the terminal reads ([blaze-protocol](../crates/blaze-protocol)):
the daemon follows each worker's events file and keeps every build's records. One more
record, `Event::Fns`, carries the functions: the codegen says each function once as it puts it
in the object (`crates/hir-ty-codegen/watch.rs`), with

| field   | what                                                                       |
|---------|----------------------------------------------------------------------------|
| `id`    | its number in the object; `calls` of the object's other functions name it  |
| `key`   | the same for the function in every build of the worker process             |
| `krate` | the crate it is written in (or compiled for, for drop glue and shims)      |
| `name`  | how a backtrace names it                                                   |
| `calls` | the object's functions it calls, by `id`                                   |
| `how`   | `Compiled { ns }`, `Unit` (an LLVM unit's), `Unchanged`, `Stored`          |

A cold build says every function of the program; a build of an edit says only those it put in
the patch. The daemon keeps one graph per workspace, merging each build's functions into it
by `key`, so the page of an edited program still shows all of it.

## The wire

The server listens on `127.0.0.1` on a port the system picks, under a random token:

    http://127.0.0.1:<port>/<token>/        the page
    http://127.0.0.1:<port>/<token>/<file>  its scripts and styles, built into blaze
    ws://127.0.0.1:<port>/<token>/ws        the build

A request with another token, a `Host` that is not `127.0.0.1:<port>` or `localhost:<port>`,
or a websocket whose `Origin` is not the page's own is refused (403). The websocket's
messages from the server are binary:

1. The first is the **snapshot** of the graph of the workspace last built, in the layout below.
2. Then the records of that workspace's last build (without its `Fns`, which the snapshot
   has), and each record of every build from then on as it comes: one CBOR item of
   [`Record`](../crates/blaze-protocol/src/events.rs) per message, in the deterministic
   encoding of the socket, without the socket's length prefix.
3. A build of another workspace sends a new snapshot first.

What the page sends is ignored but for a close.

### The snapshot

Little-endian throughout. A `str` is a `u32` byte length and that much UTF-8.

    magic     "BLZW"
    version   u32                   1
    root      str                   the workspace
    build     u64                   the daemon's number of its last build
    crates    u32 n, n str          the crates, by index
    nodes     u32 n, then n of:
      key     u64
      krate   u32                   index into crates
      how     u8                    0 Compiled, 1 Unit, 2 Unchanged, 3 Stored
      ns      u64                   compile time when Compiled, else 0
      build   u64                   the build that last put it in the program
      name    str
    edges     u32 n, then n of:
      from    u32                   index into nodes
      to      u32

The functions of a later `Fns` record are matched to the snapshot's by `key`; their `calls`
name functions of the same `object` by `id`.
