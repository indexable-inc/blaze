---
status: current
summary: The disk store every daemon shares and the remote (a directory or S3) machines push to and pull from.
---
# The disk store and the remote

blaze keeps compiled functions in memory while a daemon runs. Two more layers keep them
longer. The disk store is one directory on this machine that every daemon and every
checkout of the user shares, on by default. The remote is a directory or an S3 bucket that
several machines push to and pull from, off until you configure it.

The reports behind this file are [reports/DISKCACHE.md](reports/DISKCACHE.md) and
[reports/REMOTE.md](reports/REMOTE.md). The variables and commands below were checked
against `crates/blaze-store/src/lib.rs` and `crates/blaze-cargo/src/remote.rs`.

## What is stored and under what key

The store holds compiled functions: machine code with its relocations, unwind information
and line tables. It does not hold rust-analyzer's analysis, which is rebuilt when a daemon
loads a workspace.

A function's key is a SHA-256 over the backend and optimisation level, the target triple,
whether the build has debug info and overflow checks, and the function's stable name. The
stable name holds the function's path, its generic arguments spelled out, and its crate. A
type whose spelling would be longer than 1 KB is written as the SHA-256 of that spelling
instead: a closure that wraps closures holds each of their types twice, so written out in
full its name doubles with each level (helix's parser combinators made one of 665 MB). A
dependency is named by rustc's stable crate id (name, version, and the metadata cargo
passes). A crate of your checkout is named by a hash of its source, so two checkouts with
the same source share entries wherever they are on disk. An edit gives new names to
everything in the edited crate and in the crates of the checkout that depend on it, so
what the store serves across edits is the code of crates the edit does not reach:
dependencies, their generic instances, and the untouched crates of the workspace. Within
one running daemon, unchanged functions of an edited crate are kept by the database
instead.

An entry of the checkout also holds its mark: a hash of its code and of what it calls and
reads, by the same stable names, with the crates of the checkout named as they were when
the daemon took it. The base an edit is patched against compares the mark of the code now
with it, so a daemon that starts on a store holding the checkout's code takes that code as
it is, and the first edit recompiles only what it touched. Before, those marks hashed ids
of one process, and a started daemon compiled all it took of the checkout again before the
first edit could be a patch (nushell: 73,586 functions, 9.4 s). A program that links
rustc's code beside blaze's, an LLVM build and `#[blaze::cache]` functions keep no mark
and still take that build ([#105](https://github.com/indexable-inc/blaze/issues/105)).

Before a record is written, the store hashes that key together with the store format, what
blaze was built from and the toolchain's commit. What it was built from is a hash
`crates/blaze-engine/build.rs` takes of every file of the workspace's crates and the vendored
ones, of `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` and `harness/toolchain`, of the
features, the target and whether debug assertions are on. An entry is
therefore found by any build of blaze from the same sources on any machine, for the same
toolchain: a CI runner that builds blaze for a commit that changes only the harnesses or
the docs reuses the store of the commit before (`harness/storebuilds.py`). A blaze built
outside this workspace (the published crate) keys by the bytes of its executable, as every
build did before. Upgrading blaze makes the old
entries unreachable; they are removed over time as least recently used.

A build script's run is kept under a key with the checkout and the target directory apart:
its package's directory and the target directory its `OUT_DIR` is in are named by what stands
for them in the key and in what it printed and wrote, and put back as the checkout that reads
it has them. A run that wrote a file that is no text naming either is kept for its own place
alone, since a path of another length would break that file. The key still hashes the
script's executable, which differs between target directories (what blaze links differs
with what the store had, and an rlib holds the `OUT_DIR` an `include!` named), so a second
checkout runs its scripts again until a script is keyed by what it was built from
([#105](https://github.com/indexable-inc/blaze/issues/105)).

`harness/keys.py` tests the keying: it changes, one at a time, each thing code can depend
on that is not the text of its file, and checks that a build with a store filled before the
change still prints what cargo's build prints.

### Proc-macro expansions

The store also keeps what derive and attribute macros expand to, so a daemon that loads a
workspace again does not run them again (`crates/blaze-expand`). This output cache applies
to native proc macros. Default WASM proc macros retain session state and rerun expansions
at every new database revision, including client requests without source changes ([cargo.md](cargo.md#proc-macros-run-as-webassembly)). The key is a SHA-256 of
the macro library's content, the macro's name and kind, its input with each span replaced
by its place in the call's table of spans, that table (each span's byte range within the
item it is anchored to, and which of the call's anchors and hygiene contexts it has), the
environment and the current directory the macro is given. The output is kept with its spans
written the same way, so tokens the macro made from a string at the call site come back
anchored where the next call is. An expansion is not kept when it failed, when it asked
rust-analyzer anything while it ran (a span's text, file or line), or when its output has a
span anchored where no span of its input is.

A macro that reads a file or an environment variable by itself is pure to the key and is
not: name it in `BLAZE_PROC_MACRO_CACHE_SKIP` (a comma-separated list of macro names) to
expand it every time. `BLAZE_PROC_MACRO_CACHE=all` keeps function-like macros too, which
are off by default because they are the ones that read outside their input (`sqlx::query!`,
`include_dir!`). `BLAZE_PROC_MACRO_CACHE=check` expands every call anyway and says which
macro expanded otherwise than it was kept. `BLAZE_PROC_MACRO_CACHE=off` keeps nothing. The
daemon's log has a line per load: how many expansions ran and in how long, how many came
from the store, and why the rest were not kept. rustc's own cache, `-Zcache-proc-macros`,
keeps derives only ([research/proc-macro-cache.md](research/proc-macro-cache.md)).
`harness/expansions.py` tests it.

## The disk store

The store is `blaze/store` in the user's cache directory (`$XDG_CACHE_HOME`, else
`~/.cache`), whatever the checkout: two checkouts, or two projects with dependencies in
common, keep a function once. `BLAZE_CACHE` names another directory, or `off` for none.
The daemon reads the variable when it starts, so under `blaze` it has to be set in
the environment of the command that starts the daemon; run `blaze stop` first if
one is running. `blaze status` says how much the store holds, the most it may hold,
and where it is. A setting under its name from before the rename stops blaze, which says what
it is called now ([rename.md](rename.md)).

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_CACHE` | the store's directory, or `off` | `blaze/store` in the user's cache directory |
| `BLAZE_CACHE_MAX` | the most the sealed packs may take: bytes, or a number with `K`, `M` or `G`, or `off` | a tenth of the disk the store is on that is free or the store's, at least 256M and at most 32G |
| `BLAZE_CACHE_SEAL_AT` | size at which an open pack is sealed at the end of a build; exists for the harnesses | 4M |

The default limit is worked out again at each collection, so a disk that fills up with
other things takes room back from the store.

### What is on disk

The store is one directory. `version` names the format. `<sha256>.pack` is a sealed pack,
named by the hash of its bytes and never changed afterwards. `<pid>-<nanos>.open` is the
pack one daemon is appending to, held locked. `*.tmp` is a file on its way to another name.
`lock` is held while the directory is emptied or collected.

A sealed pack is an index, then the values. The index has, for each value, its key, its
block, where it starts in the block, its length and a checksum. The values are in blocks of
about 128K, each compressed on its own with zstd (level 3), so reading a value reads and
unpacks its block. A daemon keeps the last 32M of blocks it unpacked, since a build reads
values in about the order it added them. A value larger than 1M is a block of its own,
kept as it is, and so is a block that compression would not make smaller. An open pack is
not compressed: its records are appended as they come, and it is compressed when it is
sealed. Compiled code compresses to about a fifth: the 1.6 GB store of a helix build is
about 0.3 GB.

A daemon that finds an older format, or files with no `version`, empties the directory and
starts over. One that finds a newer format leaves it alone and runs without a store.

### Integrity and crashes

Opening the store reads the index at the start of each pack and none of the values, and a
process keeps 16 bytes of each key in memory (the first 8 bytes of the key, the pack and
the entry), reading the rest of the entry from the pack when it looks the key up. A daemon
checks the hash of every pack once it starts, beside loading the workspace; any other
process, such as the wrapper cargo starts for each build script, checks a pack's hash the
first time it finds a key in it. Each value carries a checksum that is checked every time
it is read. A pack that fails either check, or has a
block that does not unpack, is removed and what was in it is compiled again. A sealed pack
appears by a rename after it is written and synced, and the open pack it was sealed from is
removed after that. An open pack left by a daemon that died is sealed up to its last whole
record by whoever finds it.

Several daemons can share one directory. Reading and adding take no lock; only collection
and a change of format do.

If the disk fills, the daemon prints `store: nothing more is kept of this build: ...`
once, keeps compiling, and tries again on the next build. If the directory cannot be used
at all it prints `store: not used: ...` and runs without a store.

### Size and collection

A collection runs when a daemon opens the store, after it seals a pack, and on
`blaze-engine cache gc`. It seals open packs left behind, removes old temporaries, merges sealed
packs smaller than 4M into one when there are more than 16 of them, and then removes the
least recently used packs until the rest fit under `BLAZE_CACHE_MAX`. A pack counts as used
when a daemon read a value from it during a build.

Eviction is by pack, not by record. One recently used record keeps its whole pack, which
holds up to 64M of values (some 13M on disk), so a limit smaller than a few packs evicts
nearly everything. The limit counts bytes on disk, compressed.

Upgrading blaze leaves the old build's entries in the store, unreachable, until they are
the least recently used; `blaze-engine cache gc --max 0` empties the store at once.

### Commands

These act on the store `blaze status` names.

    blaze-engine cache stats                 format, packs, bytes, limit, oldest and newest use
    blaze-engine cache verify                read everything; exit 1 if a pack is not what was written
    blaze-engine cache gc [--max SIZE]       collect now, to BLAZE_CACHE_MAX or to SIZE

### Limits

- The store harnesses run in CI on Linux, Windows and macOS. `storefull.py` (a full disk)
  runs on Linux only. On Windows a pack that another process has open cannot be removed,
  so eviction of a pack in use fails and the pack is kept; this is not measured.
- Power loss was not tested. The harness kills the daemon, which does not lose the page
  cache.
- The locks are advisory, and behaviour over NFS is not tested.

## The remote

A remote is a directory, `s3://bucket/prefix`, an `https://` address it is only read from,
whose files are named by their paths with `.` for `/` (a GitHub release's assets: see "The
standard library" below), or a tenant of blaze's cache gateway,
`blaze+https://<gateway>/t/<tenant>/v3` ([remote-cache.md](remote-cache.md), the API in
`tools/remote-cache/API-v0.md`). The gateway's token is read from `BLAZE_REMOTE_TOKEN`, or
from the variable `?token=env:NAME` names; without one the tenant is read anonymously, which a
public tenant allows, and nothing is sent. Packs of 16 MB and more are fetched from it in ranges
of 8 MB, `BLAZE_REMOTE_JOBS` at a time, and objects over 90 MB go up in parts. A gateway is not
listed, so `blaze-engine cache gc --remote` does not apply to it. It holds sealed packs under the hash of
their bytes, a list of the keys in each pack, and one manifest per project that lists the
packs that project's builds were served from.

    packs/<sha256>.pack          a pack as the store has it (its blocks are compressed)
    packs/<sha256>.pack.zst      a pack sent compressed whole: one a quarter smaller or more
                                 that way, as one with values over 1 MB is (those are kept
                                 uncompressed in the store, to be read in place)
    keys/<sha256>                the keys of one pack
    manifests/<project>          one line per pack
    units/<sha256>, deps/<project>   cargo's builds of dependencies, see below

**Real AWS S3 has not been run.** Everything was measured against MinIO and SeaweedFS on
the same machine over loopback. The client is the AWS SDK, so the code is the same, but
virtual-hosted addressing, IAM errors and role credentials are untested.

### By hand

    blaze-engine cache push REMOTE [PROJECT]
    blaze-engine cache pull REMOTE [PROJECT]

`push` sends the packs the manifest lacks and adds them to it. `pull` fetches the packs the
manifest lists that are not in the local store, as many as fit under `BLAZE_CACHE_MAX`.
`PROJECT` picks the manifest and defaults to `default`. Each prints one line of what it moved,
whether it went through or not, for a CI job's summary:

    pull: 12 objects, 48211022 bytes, 1.84 s, 25.0 MiB/s

### From the daemon

With `BLAZE_REMOTE` set, the daemon pulls as it starts, pulls again when a build misses a
key and the manifest has changed, and pushes what it compiled after the first build, then
every five minutes, and once more when it is told to stop. No build waits for the network:
the first build compiles from the start while the pull runs beside it, and once the pull is
in, the store reads it and what the build asks for after that is found rather than compiled.
With `BLAZE_REMOTE_LAZY`, the first key a build lacks of a pack starts fetching that pack,
and the thread that asked waits for it a quarter of a second at most. Once a pack takes
longer, no thread of the daemon waits on the remote again: what it lacks is compiled here
while the packs come. Only `BLAZE_REMOTE_REQUIRED` has the first build wait for the pull,
since its failure is then the build's. A connection that is not made within 2 seconds fails,
as a network that is down does. A daemon killed with SIGKILL pushes nothing on its way out;
the next push sends it.

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_REMOTE` | the remote the daemon keeps its store in step with | not set |
| `BLAZE_PROJECT` | which manifest | `default` |
| `BLAZE_REMOTE_READONLY` | never write to the remote | off |
| `BLAZE_REMOTE_LAZY` | fetch only the key lists, and a pack when a build first asks for a key in it | off |
| `BLAZE_REMOTE_PUSH_EVERY` | how often a running daemon pushes (`90s`, `5m`, `1h`), or `off` | `5m` |
| `BLAZE_REMOTE_JOBS` | requests in flight at once | 8 |
| `BLAZE_REMOTE_TIMEOUT` | seconds a request may go without an answer | 30 |
| `BLAZE_REMOTE_SIGN` | file holding the key this machine signs manifests with | not set |
| `BLAZE_REMOTE_TRUST` | public keys this machine accepts manifests from, comma separated | not set |
| `BLAZE_REMOTE_TOKEN` | the bearer token of a `blaze+` gateway remote | not set |
| `BLAZE_REMOTE_REQUIRED` | a remote that fails fails the command (see below) | off |

`BLAZE_REMOTE` needs the local store (`BLAZE_CACHE` not `off`): the remote fills and
reads it. `BLAZE_REMOTE=off` turns it off for one command.

A remote that cannot be reached, answers 5xx after its retries, refuses the token (401, 403),
or sends bytes that do not hash to their name does not stop a build: what it would have
served is compiled, and every build's `Finished` line says so until an exchange with it
succeeds again, as in `[blaze, remote down: <remote>: <why>]`. With `BLAZE_REMOTE_REQUIRED=1`,
which CI sets, each of those is an error that fails the command (`cache push`, `cache pull`,
or the build), so a cache that stopped working is not a slowdown nobody sees.

### The standard library

A build with nothing of rustc's (a build script, `--exe` with every crate blaze's) starts
from the standard library compiled by blaze, which takes seconds on many cores. A release
ships it: `blaze-engine std REMOTE TOOLCHAIN` builds it for the binary and the toolchain at that sysroot and pushes it as
the project `std-<target>-<build>`, and the release attaches that remote's files. Such a
build fetches the project once per store, from:

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_STD_FROM` | the remote to fetch the standard library from, or `off` | the binary's own release, when it is one; otherwise none |

The build never waits for it. It compiles from the start, and the fetch runs beside it; once
the fetch is in, the store reads it, and what the build asks for after that is found rather
than compiled. A fetch that comes in after the build is kept for the next one. Once a store
holds the project it is not asked for again (`fetched-<project>` in the store's directory),
so a warm machine with no network builds as fast as with one. A connection that is not made
within 2 seconds fails the fetch, which says so in one line and is tried again an hour later.

What is public is fetched by default. A workspace turns that off in its root manifest:

    [workspace.metadata.blaze]
    fetch-public = false   # no standard library from the release; BLAZE_STD_FROM still names one

A build of another binary (crates.io, nix, your own) finds nothing for its `<build>` and
compiles the library itself, once per store.

### S3 address and credentials

    s3://bucket/prefix?endpoint=URL&region=NAME&profile=NAME&path-style=true|false

Whatever the address leaves out comes from where the AWS tools look: `AWS_ENDPOINT_URL`,
`AWS_REGION`, `AWS_PROFILE`, `~/.aws/config`. An unknown option is refused by name. With an
endpoint of its own, blaze addresses the bucket by path unless told `path-style=false`.

blaze does not hold credentials; the AWS SDK finds them (environment, shared config and
profiles, web identity, container and instance roles). They are not printed. The harness
searches everything blaze printed in a run for the secret key.

### Trust and signing

There are two questions.

Can the storage change the code? No. A pack is named by the SHA-256 of its bytes and is
hashed after the fetch and before it is put in the store. A remote, proxy or disk that
returns other bytes has them thrown away and reported, and the build compiles those
functions itself. Key lists and cargo units are checked the same way.

Can someone with write access make blaze use the wrong code? Without signing, yes. The
manifest is the one mutable object. Somebody who can write to the bucket can upload a
well-formed pack with wrong code under real keys and list it. Content addressing does not
catch that.

Signing does. Make a key and note the public half it prints:

    blaze-engine cache keygen ~/.config/blaze/sign.key

A machine with `BLAZE_REMOTE_SIGN` naming that file signs each manifest it writes
(Ed25519, over the manifest's name and body). A machine with `BLAZE_REMOTE_TRUST` listing
public keys refuses any manifest not signed by one of them. A machine that trusts keys but
has none to sign with refuses to push.

So: with `BLAZE_REMOTE_TRUST` set, nobody without a trusted signing key can make blaze emit
code other than what a local build gives, whatever they do to the bucket. Without it,
whoever can write to the bucket is trusted, as with sccache or a registry mirror. A trusted
signer is trusted completely; blaze does not compile again to check them. Under all of it
sits one assumption: two machines with the same blaze binary compile the same key to
equivalent code.

The harness checks, against both servers: a pack truncated, extended, emptied or with one
byte flipped; a different valid pack under another's name; a listed pack that is missing;
and manifests that are forged, unsigned, or signed by an unlisted key.

There is no `SECURITY.md` in the repository at the commit these docs were written against.

### Collecting a remote

    blaze-engine cache gc --remote REMOTE [--unlisted-after AGE] [--older-than AGE] [--dry-run]

An age is a number and `s`, `m`, `h` or `d`. The run first takes lines out of manifests:
with `--older-than`, the lines of packs written longer ago than that, and always the lines
of packs whose every key is also in other packs of the same manifest. It then removes
objects no manifest names that were written longer ago than `--unlisted-after`, which
defaults to a day and may not be less than two hours.

It is safe against a push in progress by the clock, not by a lock: a push relies on an
unlisted object only for an hour after it was written. A single push that takes more than
an hour between sending a pack and listing it, or a machine whose clock is hours off, is
outside what it promises. Nothing runs a remote collection on its own.

### Dependencies built by cargo

    blaze-engine deps push REMOTE DIR [PROJECT]
    blaze-engine deps pull REMOTE DIR [PROJECT]

Run in the directory of `Cargo.lock`, where `DIR` is the directory cargo builds a profile
in, such as `target/debug`. This sends and fetches what cargo built of registry and git
dependencies (rlibs, build script output, fingerprints) so that a fresh checkout's first
cargo build finds them fresh. It is separate from the function store and is tested by
`harness/deps.py`.

### What it buys, measured

On one machine over loopback, a fresh second "machine" (another home directory, checkout
and store) building ripgrep 14.1.1 took 16.8 s to a running program with no remote, 14.3 s
with blaze's packs, and 9.6 s with packs and cargo's units. For cargo 0.92.0 the same three
were 152.4 s, 117.1 s and 94.4 s. In each case with a remote, blaze compiled 0 functions.
What is left is cargo running build scripts and proc macros, and blaze loading the
workspace and walking every instance to find its key. Lazy fetching was slightly slower
than fetching everything in both. Transfer times over loopback are a lower bound and say
nothing about a real network. The full tables are in [results.md](results.md).

### Limits

- Real AWS has not been run.
- Windows and macOS were not run for the remote; the harness is Linux only.
- When one fetch runs out of retries the remote is taken to be down and that process's
  other transfers stop. The next run resumes them.
- A merged pack is held twice on the remote until someone runs `cache gc --remote`.
