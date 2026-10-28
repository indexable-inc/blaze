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
record, `Event::Functions`, carries the functions: the codegen says each function once as it puts it
in the object (`crates/hir-ty-codegen/watch.rs`), with

| field   | what                                                                       |
|---------|----------------------------------------------------------------------------|
| `id`    | its number in the object; `calls` of the object's other functions name it  |
| `key`   | the same for the function in every build of the worker process             |
| `krate` | the crate it is written in (or compiled for, for drop glue and shims)      |
| `name`  | how a backtrace names it                                                   |
| `calls` | the object's functions it calls, by `id`                                   |
| `how`   | `Compiled { ns }`, `Unit` (an LLVM unit's), `Unchanged`, `Stored`          |

A record is of one `object` and says whether it is `whole`. A whole object (a cold build,
or a build that wrote the program again) says every function of the program, and begins the
workspace's graph again; a patch (a build of an edit) says only those it compiled again,
which are merged into the graph by `key`, so the page of an edited program still shows all of
it.

The daemon keeps the program of each of the 16 workspaces built last in a
[Flecs](https://www.flecs.dev) world, on a thread of its own that the first record starts
(`crates/blaze-web/src/world.rs`; Flecs's C is `vendor/flecs-c`):

| entity    | what                                                                       |
|-----------|----------------------------------------------------------------------------|
| workspace | `Workspace { root, build }`, unnamed                                       |
| crate     | `Crate`, named as the crate, `(ChildOf, workspace)`                         |
| module    | `Module`, named as the module, `(ChildOf, crate)` or of the module it is in |
| function  | `Function { key, name, how, ns, build }`, unnamed, `(ChildOf, module)`, `Callees`: the functions it calls |

A function is in the module its path names: its name without generic arguments
(`<alloc::vec::Vec<u8>>::push` is `alloc::vec::Vec::push`), the segments after its crate's
that begin with a lowercase letter (a function with none is in its crate). A module one of
whose functions calls one of another's has `(Calls, that module)`, and a crate so
`(Calls, that crate)`; such a pair stays until the program is written again. The calls of
functions are a vector, not pairs: anim's 150 000 as pairs would hold 266 MB (1.9 GB as pairs
that do not fragment). Every view of the page is a query of this world, and
so can be any other (`/flecs/` below).

## The wire

The server listens on `127.0.0.1` on a port the system picks, under a random token:

    http://127.0.0.1:<port>/<token>/        the page
    http://127.0.0.1:<port>/<token>/<file>  its scripts and styles, built into blaze
    ws://127.0.0.1:<port>/<token>/ws        the build
    http://127.0.0.1:<port>/<token>/flecs/… Flecs's REST over the world, read only

A request with another token, a `Host` that is not `127.0.0.1:<port>` or `localhost:<port>`,
or a websocket whose `Origin` is not the page's own is refused (403). The websocket's
messages from the server are binary:

1. The first is the **snapshot** of the program of the workspace last built, in the layout
   below, gzipped (the page reads it with `DecompressionStream("gzip")`): anim's 80 000
   functions are 37 MB of it, nearly all names, and 4.3 MB gzipped.
2. Then the records of that workspace's last build (without its `Functions`, which the snapshot
   has), and each record of every build from then on as it comes: one CBOR item of
   [`Record`](../crates/blaze-protocol/src/events.rs) per message, in the deterministic
   encoding of the socket, without the socket's length prefix.
3. A build of another workspace sends a new snapshot before its `Began`, and the first record
   of a whole object one in place of the record (its functions are in the snapshot): the page
   starts over from it. A message is
   a snapshot when it begins with gzip's `1f 8b`, which no record does (a record is a CBOR
   map).

What the page sends is ignored but for a close.

`/flecs/` passes the rest of the path, with its query, to Flecs's REST
([its API](https://www.flecs.dev/flecs/md_docs_2FlecsRemoteApi.html)), which answers on the
world's thread: `/flecs/query?expr=Function` is every function, `/flecs/query?expr=Module,(Calls,*)` what each
module calls. Only `GET` is let through, and the REST server is made read only besides
(`glue_rest_new`), so a page or a query never changes the world. Flecs's own port is never
opened: the REST is behind the token and the checks above like the rest of the page.

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

The functions of a later `Functions` record are matched to the snapshot's by `key`; their `calls`
name functions of the same `object` by `id`.
