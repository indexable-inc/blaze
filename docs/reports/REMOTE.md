---
status: record
summary: Remote hydration over S3.
date: 2026-10-20
---
# Remote hydration over S3

Branch `blaze-s3`, from `blaze-collate` at `27315323cb`, with `blaze-collate-wip` at
`0151c01218` (debug info, build model, disk cache) and then `blaze-collate` at `1547078a30`
merged in. The numbers below are from the third round (compressed packs) unless they say
otherwise.

`blaze-engine cache push REMOTE` and `blaze-engine cache pull REMOTE` had an S3 path that had never been
run. This is what it took to make it real, what was measured, and what is still open.

**Real AWS S3 was not run.** There are no AWS credentials in the session this was done in.
Everything below was measured against MinIO (`RELEASE.2026-09-22T19-25-18Z`) and SeaweedFS
(4.48) on the same machine, over loopback. The harness runs against a real bucket with one
variable, `BLAZE_S3_REMOTE=s3://bucket/prefix`; nobody has done that yet.

## What changed

The S3 client is now the AWS SDK for Rust (`aws-config`, `aws-sdk-s3`) in place of
`rusty-s3` and `ureq`. The reason is credentials: `rusty-s3` signs with a key pair you hand
it, and the task asks for the sources the AWS tools read. With the SDK's default chain blaze
takes credentials from the environment, the shared config and credentials files (and a
named profile), web identity tokens, ECS container credentials and EC2 instance roles,
without blaze having any code for them. Of those, the environment and the shared files with a profile
were run; web identity, container and instance roles were not, because there is nothing here
that issues them.

The code is in `crates/blaze-engine/src/remote.rs` and three files under `crates/blaze-engine/src/remote/`:

- `s3.rs`: the S3 calls. Ranged GET, HEAD, conditional PUT for lists, multipart upload.
- `transfer.rs`: fetching an object into a file. Resuming, hashing, the limit on requests
  in flight, the counters.
- `signed.rs`: signed manifests.

`crates/blaze-engine/src/deps.rs` (`blaze-engine deps push|pull`, cargo's compiled units) moved onto the
same remote, so it has the same verification, retries and counters.

### On the store of the disk-cache branch

The merge put the remote on the format-2 store. What that means in the code:

- The remote goes through `StoreDir` for everything it does to the directory: `claim`
  before a push or a pull (so a store of an older format is emptied and one of a newer
  format is refused, as for a daemon), `adopt` before a push, `packs` for what is there.
- A pack's keys for the remote come from `pack_keys`, which is the store's own check of
  the pack (magic, name, every record's checksum), so a pack the store would discard is
  never sent. The keys are the ones on disk, which already have the store's identity
  (format, blaze build, toolchain) hashed in.
- There is one set of crash-safe writes. A fetched pack arrives in pieces, so it cannot be
  handed to `write_file_whole` as bytes; `store.rs` now has `name_file_whole`, which is the
  second half of its `write_whole` (sync, rename, sync the directory), and `write_whole`
  itself is written with it. The remote's partial file is `<hash>.pack.fetch.tmp`, which
  the store counts as one of its temporary files: `cache stats` shows it, and a collection
  removes it once it is a minute old and nobody holds it. The directory remote writes its
  objects with `write_file_whole` and `name_file_whole` too. The `.partial` files and the
  rename of the first version are gone.
- A manifest line is now `PACK SIZE KEYS FORMAT`. A pull passes over packs listed with
  another format without fetching them ("N of another format left"). A line that claims
  this format for a pack that starts with another magic is not believed: the pack is
  rejected after the fetch and never gets its name.
- `BLAZE_CACHE_MAX`: a pull fetches the listed packs that fit under the limit and says how
  many do not ("N left that do not fit under BLAZE_CACHE_MAX"), then runs the store's
  collection, which evicts the packs unused the longest if the store is now over. A pulled
  pack is a pack like any other after that. One that is evicted is missing again, so the
  next pull fetches it.
- A collection also makes one pack of many small ones, under a new name. So "is this
  listed pack here" cannot be asked by name alone, or a machine would fetch its own
  history again at every start. When the store is not empty and a listed pack is missing
  by name, the pull fetches its key list and passes the pack over if every key is in the
  store already (`StoreDir::keys`). A fresh machine has no keys and makes no such request.
- Lazy hydration keeps the key lists in memory. Nothing that is not the store's is written
  into the store's directory, so `cache stats` and `cache verify` count the same things
  after a pull as before.
- `blaze-engine cache stats|gc|verify` are the disk-cache branch's, unchanged. `cache keygen` is
  next to them.

New public items in `hir-ty`'s `codegen`, since that is an architectural change in this
tree: `name_file_whole`, `pack_keys`, `pack_format`, `StoreDir::keys`, and
`fetch_missing_with`/`FetchMissing` for lazy hydration.

### What the AWS SDK costs

Measured on this machine (8 cores), `cargo build -p blaze-engine --release` into an empty target
directory with the crates already downloaded, Cranelift only:

| | crates in blaze's tree | clean build | binary | stripped |
| --- | --- | --- | --- | --- |
| `blaze-collate-wip` (`rusty-s3`, `ureq`) | 284 | 91.7 s, 85.5 s | 45.3 MB | 32.5 MB |
| the SDK with its default features | 380 | 129.2 s | 68.6 MB | 47.3 MB |
| the SDK as it is on this branch | 347 | 113.5 s | 62.6 MB | 42.5 MB |

So the SDK adds 63 crates, about 25 s to a clean build (roughly 30%) and 10.1 MB to the
stripped binary (31%). The first row was built twice and the other two once, so the build
times are good to a few seconds.

The default features cost more than that, and the branch turns three of them off:
`rustls` on `aws-sdk-s3` is a second, older HTTP and TLS stack (hyper 0.14, rustls 0.21)
next to the one it actually uses; `default-https-client` brings aws-lc, a C library built
with cmake; `sigv4a` is only for multi-region access points. The client is built by hand
instead, hyper 1 over rustls with ring, both of which `ureq` had in the tree already. TLS
through that client was run against a MinIO with a certificate (see the harness), not
against AWS.

What is left of the cost is mostly generated code: the S3 client, STS for web identity
and role credentials, and the two SSO clients. Dropping the `sso` feature saves another
1.3 MB stripped and 3 crates; it is kept because `aws sso login` is how most people get
credentials on a laptop. If 10 MB is too much, the alternative is `rusty-s3` for signing
plus `aws-config` only for credentials, or credentials from the environment only as
before. That would lose nothing that was measured here, but blaze would own the request
code (multipart, retries, error parsing) that the SDK owns now.

The remote is async on one tokio runtime of two worker threads, built once in a process.
The daemon's own threads are synchronous and wait on that runtime from outside; nothing
calls `block_on` from inside a task.

### Layout of a remote

    packs/<sha256 of the pack>.pack.zst    the pack, compressed with zstd
    keys/<sha256 of the key list>          the 32-byte keys of one pack, back to back
    manifests/<project>                    one line a pack: PACK SIZE KEYS FORMAT ZDIGEST ZSIZE
                                           and, when signed, a last line: signed KEY TURN SIGNATURE
    units/<sha256>, deps/<project>         cargo's units, the same way

### Address and configuration

    s3://bucket/prefix?endpoint=URL&region=NAME&profile=NAME&path-style=true|false

Anything the address leaves out comes from where the AWS tools look: `AWS_ENDPOINT_URL`,
`AWS_REGION`, `AWS_PROFILE`, `~/.aws/config`. An unknown option is refused by name. With an
endpoint of its own blaze addresses the bucket by path unless told `path-style=false`,
because that is what most services that are not AWS need; without one it does what the SDK
does for AWS.

| Variable | Meaning | Default |
| --- | --- | --- |
| `BLAZE_REMOTE` | the remote the daemon pulls from before a build and pushes to after | none |
| `BLAZE_PROJECT` | which manifest | `default` |
| `BLAZE_REMOTE_JOBS` | requests in flight at once | 8 |
| `BLAZE_REMOTE_TIMEOUT` | seconds a request may go without an answer, or a body without a byte | 30 |
| `BLAZE_REMOTE_READONLY` | never write to the remote | off |
| `BLAZE_REMOTE_LAZY` | fetch a pack when a build first asks for a key in it | off |
| `BLAZE_REMOTE_PUSH_EVERY` | how often a daemon that stays pushes (`90s`, `5m`, `1h`), or `off` | `5m` |
| `BLAZE_REMOTE_SIGN` | file with the key this machine signs manifests with | none |
| `BLAZE_REMOTE_TRUST` | public keys this machine takes manifests from | none |
| `BLAZE_REMOTE_UNSIGNED` | `1`: with no keys named, take lists nobody signed | off: a pull is refused |
| `BLAZE_REMOTE_MAX` | the most taken of one object that is neither a list nor a pack | 512M |

In read-only mode `cache push` and `deps push` fail with "BLAZE_REMOTE_READONLY is set:
nothing is sent", and the daemon does not push.

Credentials are never printed. blaze does not hold them; the SDK does. Errors are printed
as the service's error code and message, or the HTTP status, or the chain of causes of a
transport error, none of which contain the request's headers. The harness greps everything
blaze printed in the whole run for the secret key (and the session token when run against a
real bucket), forwards and reversed.

## Robustness

**Fetch.** An object is written to `<name>.fetch.tmp` under an exclusive lock, never to its
final name. When the body breaks off, blaze asks again with `Range: bytes=N-` from the byte
it stopped at, up to 6 times in a row without getting further, waiting 250 ms, 500 ms, ...
8 s in between; getting any bytes starts the count again. When the object is whole, its
SHA-256 is compared with the name it was listed under, and only then is it synced and
renamed to `<hash>.pack` by the store's `name_file_whole`. So a pack file in the store is
always a whole, verified pack: a killed blaze or a killed server leaves the temporary file
or nothing. The next process to run picks it up and fetches only the rest, unless a
collection got to it first (the store removes temporary files a minute after their last
byte). If what is in it turns out not to be the start of this object (the hash does not
match at the end), it is thrown away and fetched once from the start before the object is
rejected.

**Small objects** (manifests, key lists, the HEAD before a send, the conditional write of a
manifest) are asked for again on the same schedule as a pack is resumed: up to 6 more times,
250 ms to 8 s apart, on top of the SDK's own attempts. A conditional write that is made
again after its answer was lost finds its own bytes there, is told it lost, and the read
that follows sees there is nothing left to add.

**What is not tried again.** A refusal (any 4xx but 408 and 429: wrong credentials, no such
bucket), an object a list names that is not there, and an error writing the local file
(a full disk) are final. They fail at once, and the temporary file is removed: there is
nothing in it worth keeping when the disk is full. A TLS failure such as an untrusted
certificate is not told apart from a network failure and takes the whole 16 s to fail.

**Send.** A file of 16 MiB or more goes up as a multipart upload in 8 MiB parts, with at
most `BLAZE_REMOTE_JOBS` requests in flight across everything the process is doing. A part
that fails is sent again by the SDK (three attempts, exponential backoff with jitter). If
the upload fails it is aborted, and a push first aborts uploads an earlier process left
unfinished under the same key. The manifest is written last, after every pack and key list
it names is in the bucket, with `If-Match`/`If-None-Match` so that two machines pushing at
once do not undo each other (up to 8 rounds of read, extend, write).

**Timeouts.** Connecting: 5 s at most. An attempt with no answer, or a read with no bytes:
`BLAZE_REMOTE_TIMEOUT`. An upload attempt gets that plus a second for every 64 KiB, so a
slow link is not mistaken for a dead one.

**Failures are counted.** Every command ends with a line like

    remote: 15 requests (0 tried again, 0 transfers resumed), 0 bytes received, 42336756 sent, 0 objects rejected

and a push or pull in which anything failed prints each failure and exits non-zero with
"N failed, M of them rejected". In the daemon a remote that fails is not an error to the
build: it says so and compiles what it would have fetched.

## Trust

There are two different questions and they have different answers.

**Can the storage change the code?** No. A pack is named by the SHA-256 of its bytes and
is hashed after the fetch and before the rename, so a bucket (or a proxy, or a bad disk on
the way) that returns other bytes for a name gets them thrown away, counted under "objects
rejected", and reported: "rejected packs/X.pack: its N bytes hash to Y, not to X". A machine
also hashes each of its own packs before sending it and refuses to send one that a bad disk
spoiled.
Key lists and cargo units are named and checked the same way. The store checks a CRC32 of
each record again when it reads it. After a rejection the build compiles the functions
itself, so the result is the code a local build gives.

**Can the storage make blaze use code for the wrong thing?** A cache entry's key is a
SHA-256 over the backend and optimisation level, the target triple, whether the build has
debug info and overflow checks, and the function's stable name; the store then hashes that
together with its format, the blaze executable's own hash and the toolchain's commit before
it is written to disk. The stable name holds the function's path, its
generic arguments spelled out, and for the crate it is in either rustc's stable crate id
(dependencies: name, version and the metadata cargo passes) or a hash of the crate's source
(crates of the checkout). So an entry is only ever found by a machine running the same
blaze binary against the same toolchain, for the same backend and target, for the same instance of the same crate
source. A remote cannot move an entry to another key without changing the pack's bytes and
so its name.

**What is left is the manifest**, which is the one mutable object and says which packs to
fetch. Somebody who can write to the bucket can upload a well-formed pack of their own
making, with wrong code under real keys, and list it; its name is honestly the hash of its
bytes. Content addressing cannot catch that. Signing does: with `BLAZE_REMOTE_SIGN` a
machine signs each manifest it writes (Ed25519, over the manifest's object name, its turn
and its body, so one project's manifest cannot be replayed as another's), and with
`BLAZE_REMOTE_TRUST` a machine refuses any manifest that is not signed by a key it lists.
`blaze-engine cache keygen FILE` makes a key and prints the public half.

**A machine that names no keys pulls nothing** unless `BLAZE_REMOTE_UNSIGNED=1` is set:
"BLAZE_REMOTE_TRUST names no key, and without one whoever writes to the remote says what is
run here". Pushing asks for nothing. This is the default that was left open here; it was
decided in the security round (`SECURITY.md`).

**An old list.** A signature says who wrote a list, not that it is the newest. Each signed
list carries a turn, a number inside the signed bytes that is one more than the turn of the
list it replaces and never less than the signer's clock in seconds. A machine remembers the
highest turn it took of each list of each remote, in `turns/` of its store (for `blaze
deps` without a store, in `.blaze/turns` of the target directory), and refuses a list with
a lower one: "`manifests/default` is turn N, and this machine has taken turn M of it: an
older list is not taken". `blaze-engine cache forget REMOTE` removes what is remembered of a
remote. A machine that has never pulled remembers nothing and takes the first list it is
given. The design, with what it does not cover, is in `SECURITY.md`.

**How much.** A list is read to 16 MiB, a pack to the sizes its manifest gives it (the
object as sent to ZSIZE, and what it unpacks to to SIZE, so a small object that unpacks to
a great deal is stopped at SIZE), the keys
of a pack to no more than the pack, and the archive of a unit to `BLAZE_REMOTE_MAX`; a
`deps pull` unpacks no more than `BLAZE_DEPS_MAX` (64G, `off` for none). An object that is
longer is rejected and counted like one with the wrong hash, with nothing of it left on
disk.

So the honest statement is: with `BLAZE_REMOTE_TRUST` set, nobody without a trusted signing
key can make blaze emit different code than a local build, whatever they do to the bucket.
With `BLAZE_REMOTE_UNSIGNED=1` instead, whoever can write to the bucket is trusted, the same
as with sccache or a cargo registry mirror. A trusted signer is trusted completely; blaze does not recompile to check
them. One more assumption sits under all of it and predates this branch: two machines with
the same blaze binary compile the same key to equivalent code. That is what the local store
already relies on between workspaces.

Tested by the harness, on both servers:

- pack truncated, extended, emptied, one byte flipped: rejected, reported, no pack in the store;
- a different valid pack stored under A's pack name: rejected, B compiles its own, and its
  program prints what cargo's build prints;
- a pack the manifest lists that is gone: reported as "is listed and is not there", exit non-zero;
- with trust on: a manifest forged under A's old signature, one with no signature, and one
  signed by a key B does not list are each refused, nothing is fetched, and the program is
  the same;
- a machine that trusts keys but has none to sign with refuses to push;
- with no keys and without `BLAZE_REMOTE_UNSIGNED=1`: a pull is refused and fetches nothing,
  a push goes through;
- a pack listed as shorter than it is, and a manifest one byte over 16 MiB: rejected,
  nothing kept;
- a signed manifest put back after a newer one was taken: refused as older, nothing
  fetched; taken by a machine that never pulled; taken after `cache forget`; and a signer
  that pushes over it writes a new list with a later turn.

## Where the slowdown after the merge went

It is debug info. It is not the store's checksums or its syncs.

Measured with the same build of each project run six times in a row from an empty store,
changing one thing at a time (`~/work/slow.py`, not in the tree: it runs `blaze` on the
workspace with the environment given and reads the `store:` and `cold:` lines). The first
run of a series is always slower (2 s for ripgrep, 8 to 9 s for cargo) whatever it is, so
the series starts with one that is thrown away. The numbers are blaze's seconds only, not
cargo's.

| cargo 0.92.0 | blaze | of that, code generation | store |
| --- | --- | --- | --- |
| before the merge (`7b33ead6c2`) | 81.2 s | 71.0 s | 928,617,374 bytes |
| after the merge | 101.9 s | 91.6 s | 1,067,053,828 bytes |
| after, `BLAZE_DEBUGINFO=0` | 87.2 s | 77.0 s | 927,233,654 bytes |
| after, no sync at all (`LD_PRELOAD=libeatmydata.so`) | 101.9 s | 91.5 s | 1,067,053,828 bytes |
| after, both | 87.2 s | 77.1 s | 927,233,654 bytes |
| after, no store (`BLAZE_CACHE` not set) | 73.1 s | 63.3 s | 0 |

| ripgrep 14.1.1 | blaze | store |
| --- | --- | --- |
| before the merge | 9.2 s | 41,551,465 bytes |
| after the merge | 10.1 s | 51,806,179 bytes |
| after, `BLAZE_DEBUGINFO=0` | 9.1 s | 41,452,707 bytes |
| after, no sync | 10.4 s | 51,806,179 bytes |
| after, no store | 7.3 s | 0 |

So for cargo the merge cost 20.7 s of blaze. Turning debug info off gives back 14.7 s of
it and all of the 140 MB the packs grew by. Turning every sync into nothing gives back
nothing: the store syncs once a sealed pack, not once a record, and there are 16 packs.
The other 6 s are still there with debug info off and syncs off, so they are in what the
merged base does when it compiles, not in the store's policy. I did not look for them
further. For ripgrep the whole difference is debug info.

The per-record CRC was not measured on its own, since there is no switch for it. It is
bounded by the row without a store: the store as a whole (serialising, hashing and writing
927 MB of entries, CRC included) is 14 s of cargo's build after the merge, and the
format-1 store without CRCs was 22 s on the older binary (81.2 s with, 59.2 s without). So
the format-2 store is not slower than the one it replaced.

No change to the sync policy is proposed, because the measurement says there is nothing to
win there. If debug info in a dev build is wanted but its cost is not, the place to look is
whether line tables have to be in the entry of every instance.

The two totals in the last report (17.3 s and 152.0 s against 15.2 s and 132.7 s) each had
a first-run effect in them as well; the tables above compare runs that are not the first.

## Compression

Packs travel compressed with zstd at level 3. The `zstd` crate was in blaze already
(`deps.rs` packs cargo's units with it), so nothing was added.

A pack is `packs/<digest of the pack>.pack.zst`. Its manifest line carries the digest and
size of the compressed object next to those of the pack. A fetch checks the compressed
bytes against the first as they arrive, which is what a resumed transfer continues from,
then unpacks them into a second temporary file and checks what came out against the pack's
own digest and format before it gets the pack's name. So the name still says what the
content is, and an object that is a valid zstd stream of something else is rejected (the
harness has that case). A line with four fields is a plain pack as before and is still
read. Level 3 was chosen on one ripgrep pack of 51.8 MB: level 1 gave 9.4 MB in 0.09 s,
level 3 7.5 MB in 0.11 s, level 9 6.5 MB in 0.47 s; unpacking takes 0.05 s and hashing the
pack 0.14 s.

| | packs, plain | packs, zstd | push | pull |
| --- | --- | --- | --- | --- |
| ripgrep | 51,804,616 bytes | 7,354,887 bytes | 418 ms, then 432 ms | 180 ms, then 240 ms |
| cargo | 1,066,939,839 bytes | 95,041,189 bytes | 3.7 s, then 1.1 s | 3.1 s, then 1.0 s |

That is 7 and 11 times smaller. On loopback the time moves little for ripgrep (the pack is
compressed and unpacked in the time it took to send it) and is three times shorter for
cargo. The bucket for cargo, with cargo's units, went from 1,504 MB to 532 MB; the units
were compressed already. The full tables are under Measurements.

## Lazy hydration

With `BLAZE_REMOTE_LAZY=1` the daemon fetches only the key lists (32 bytes a function)
before a build. When the store is asked for a key it lacks and a listed pack holds it, the
compiling thread fetches that pack, verified as above, and looks again; other threads that
want the same pack wait for that one fetch. A pack nobody asks of is never fetched; the
harness checks that. Fetches in flight are bounded by `BLAZE_REMOTE_JOBS` like every other
request, since they take a place on the same line.

The unit that is fetched is a pack, not a record. A remote has no object for one key, and
making one would be a request per function (224,346 for cargo).

It still does not win on a fresh machine: 10.0 s against 9.6 s for ripgrep and 100.2 s
against 94.4 s for cargo, for 0.8 MB and 7 MB more (the key lists). A fresh machine
building the same workspace wants every pack, and here they arrive one after another as
threads trip over them, each fetch, unpack and check holding up the threads that wait for
it, instead of eight at a time before the build starts. That reading of where the six
seconds go is an inference; I did not profile it. So eager stays the default and lazy is
behind the setting. Where lazy should win is a manifest that holds much more than one
build needs; the only evidence for that is the harness case where the unused pack is not
fetched.

## Collecting a remote

    blaze-engine cache gc --remote REMOTE [--unlisted-after AGE] [--older-than AGE] [--dry-run]

An age is a count and `s`, `m`, `h` or `d`. The run does two things, in this order.

It takes lines out of manifests. With `--older-than`, the lines of packs written longer
ago than that. Always, the lines of packs whose every key is also in other packs of the
same manifest: that is the "merged code held twice" case, where a store pushed its small
packs, later merged them and pushed the merged one. The key lists say it, at one small
request a pack. Packs are looked at smallest first, so the merged pack stays and the ones
it was made of go. A manifest is rewritten with the same conditional write a push uses, so
a push that lands in between is not lost; the run reads again and tries again. A manifest
that cannot be trusted (signed by nobody this machine takes manifests from) is left as it
is and said.

Then it removes objects under `packs/`, `keys/` and `units/` that no manifest or unit list
names and that were written longer ago than `--unlisted-after` (a day when not given). The
time is looked at again right before each removal.

What makes it safe against a push that is under way is the clock. A push sends a pack and
its keys first and lists them last, so for a while they are there and unlisted. A push
takes an object that is already there as sent only for an hour after it was written; older
than that it sends it again, which makes it new. A collection will not take an unlisted
object younger than two hours, and refuses a smaller `--unlisted-after`. So an object a
push relies on is either under an hour old, and out of the collection's reach for another
hour, or was just written again. What this does not cover is a single push that takes
over an hour between sending a pack and listing it, and machines whose clocks disagree
with the service's by that much: the ages are the service's times against this machine's
clock.

A line that was taken out leaves its object to the second rule, so a pack that was old or
doubly held is gone a day (or the time given) after it was written, in the same run if it
is that old already.

In the harness: unlisted objects of three ages, a pusher stopped between sending and
listing (through the proxy), old listed packs, merged packs, a dry run, a manifest that
is not trusted, and a store on another machine that pulls afterwards and compiles
nothing. The proxy changes the times the service reports so that nothing has to wait an
hour. 12 of 12 on MinIO and on SeaweedFS.

## A daemon that stays

A daemon given `--socket` or `--watch` used to pull when it started and push once after
its first build. Now a thread of its own keeps the two in step
(`crates/blaze-engine/src/remote/keeper.rs`):

- It pushes right after the first build, then every `BLAZE_REMOTE_PUSH_EVERY` (five
  minutes), then once more when the daemon gets SIGTERM or SIGINT, after which the daemon
  ends by that signal as it would have. The last push is given a minute. Before a push
  the pack the store has open is sealed, however small, so an edit's one function goes.
- When a build asks the store for a key it lacks, the thread reads the manifest, at most
  once in five seconds however many keys are lacking. If it is not what it was when the
  daemon last pulled, it pulls. The build that lacked the key does not wait: it compiles
  the function. The next build finds what arrived.
- The daemon's own push changes the manifest too. That is not taken for a change, unless
  the manifest had already changed before the push.

No build waits for any of this. In the harness the proxy holds every request for three
seconds while an edit is built, and the build takes 0.11 s. The other checks: the first
build is pushed without being asked, an edit is pushed on a two-second schedule, a pack
another machine added is pulled after the next build that lacks something and not pulled
again, SIGTERM ends the daemon with exit by SIGTERM and nothing left to send, with the
schedule `off` an edit is sent only at the end, and a third machine then builds the edited
program and compiles nothing. 8 of 8 on both servers.

A build with no `--socket` and no `--watch` pushes before it exits, as before.

What it does not do: there is no push when the daemon is killed with SIGKILL or the
machine loses power; what was compiled since the last push is in the local store and goes
with the next one. On Windows only Ctrl-C is waited for, and that path was not run.

## Measurements

Machine: 8 cores, 31 GB, Linux x86-64. Cranelift backend. MinIO on loopback behind the
harness's counting proxy, so transfer times are a lower bound; they say nothing about a real
network. Each "machine" is its own HOME, checkout, cargo target directory and blaze store
in a fresh directory. They are not separate containers. One run each, no repeats, so read
differences of a few percent as noise.

"To a running program" is everything a machine with a checkout and a toolchain does: pull
cargo's units if any, `cargo rustc` for the dependencies and link line, blaze's build, run
the binary. Crates.io sources were already in `~/.cargo` for all of them.

    cd harness
    BLAZE_BIN=.../blaze python3 remote.py --measure ~/work/rg-a -p ripgrep --bin rg \
        --crate rg,grep,grep_cli,grep_matcher,grep_printer,grep_regex,grep_searcher,globset,ignore
    BLAZE_BIN=.../blaze python3 remote.py --measure ~/work/cargo-a -p cargo --bin cargo \
        --crate cargo,cargo_util,cargo_platform,cargo_util_schemas,crates_io,rustfix,cargo_credential

ripgrep 14.1.1 (9 crates by blaze; 22,340 functions compiled cold):

| Fresh machine B | To a running program | cargo | blaze | Functions compiled | Requests | Bytes down |
| --- | --- | --- | --- | --- | --- | --- |
| no remote | 16.8 s | 4.6 s | 12.2 s | 22,340 | 0 | 0 |
| blaze's packs | 14.3 s | 4.8 s | 9.5 s | 0 | 3 | 7,355,315 |
| packs and cargo's units | 9.6 s | 2.0 s | 7.3 s | 0 | 45 | 59,161,263 |
| the same, lazy | 10.0 s | 2.1 s | 7.6 s | 0 | 46 | 59,946,863 |

Machine A's push: 1 pack, 7,354,887 bytes in 432 ms, 9 requests; 41 cargo units,
51,802,194 bytes in 427 ms, 85 requests. The bucket ends with 45 objects, 59,946,649
bytes. Pulling the pack took 240 ms.

cargo 0.92.0 (7 crates by blaze; 226,180 functions compiled cold, 3,599 of 3,602 functions
of the workspace; the other three are refused by blaze and compiled by rustc, as on the base branch):

| Fresh machine B | To a running program | cargo | blaze | Functions compiled | Requests | Bytes down |
| --- | --- | --- | --- | --- | --- | --- |
| no remote | 152.4 s | 42.0 s | 110.3 s | 226,180 | 0 | 0 |
| blaze's packs | 117.1 s | 43.4 s | 73.6 s | 0 | 18 | 95,048,041 |
| packs and cargo's units | 94.4 s | 25.2 s | 67.4 s | 0 | 406 | 525,028,848 |
| the same, lazy | 100.2 s | 25.5 s | 73.1 s | 0 | 422 | 532,207,920 |

Machine A's push: 16 packs, 95,041,189 bytes in 1.1 s, 74 requests; 387 units,
429,944,962 bytes in 3.2 s, 777 requests. The bucket ends with 421 objects, 532,204,494
bytes. Pulling the 16 packs took 1.0 s.

In every row the program printed the same as the one built without the remote.

The same tables with plain packs, from the round before (same machine, same commands):

| Fresh machine B | ripgrep | bytes down | cargo | bytes down |
| --- | --- | --- | --- | --- |
| no remote | 17.3 s | 0 | 152.0 s | 0 |
| blaze's packs | 14.5 s | 51,804,757 | 114.1 s | 1,066,942,095 |
| packs and cargo's units | 9.4 s | 103,610,692 | 92.0 s | 1,496,922,653 |
| the same, lazy | 9.8 s | 104,396,292 | 98.2 s | 1,504,101,885 |

So compression cut the bytes a fresh machine takes by 43% for ripgrep and 65% for cargo
(by 86% and 91% with blaze's packs alone) and left the time to a running program where it
was, within the few percent one run differs from the next. On loopback there was nothing
to win in time: the pull was 3 s of 92 before. On a real network the bytes are the time;
that is arithmetic, not something measured here.

What the numbers say otherwise is what they said before: the remote removes all of
blaze's code generation (0 functions compiled). What is left is blaze loading the
workspace and walking every instance to find its key (the "cold:" line still reads 4.1 s
for ripgrep and 57 s for cargo with nothing to compile), and cargo running build scripts
and proc macros. Those are the next thing to cut and are not a remote problem.

## The harness

`harness/remote.py` with `harness/s3fake.py`. It starts MinIO and SeaweedFS (found through
`BLAZE_MINIO`, `BLAZE_WEED`, PATH or `~/s3tools`), each behind a proxy of its own that
counts requests and bytes and can answer with a status, reset the connection, cut a response
or a request body at a byte, stall, or throttle. It builds a small generated workspace on
machine A, and then checks, 178 checks in all across the two servers:

- the round trip: A compiles and pushes, B with another HOME and an empty store pulls,
  compiles 0 functions, and prints what cargo's build prints; a second push sends nothing;
  every object in the bucket hashes to its name;
- configuration: read-only, a profile in the shared files, `AWS_PROFILE`,
  `AWS_ENDPOINT_URL`, region and path style, the bucket in the host name (MinIO, through
  `bucket.localhost`), a wrong secret, an unknown option, no secret in any output;
- TLS (MinIO only): a second MinIO behind a certificate signed by an authority the harness
  makes with `openssl`; a 20 MiB pack goes up and comes down with the authority trusted
  through `SSL_CERT_FILE`, and without it the pull is refused and says "invalid peer
  certificate", in under three seconds;
- the store after a pull: `cache verify` and `cache stats` count exactly what was fetched;
  packs evicted with `cache gc --max 0` are fetched again; with `BLAZE_CACHE_MAX` a pull
  fetches the three packs of six that fit and says so; into a store that already holds two
  old packs it fetches three and the two old ones are evicted; 24 small packs that a
  collection made one of are not fetched again; a pack of format 1 listed as format 1 is
  left with no request made for it, and the same pack listed as format 2 is rejected;
- a fetch killed half way leaves a store that verifies and counts the kept bytes as one
  temporary file;
- multipart with bounded concurrency, and blaze's request count equal to the proxy's;
- 503 twice, 503 on a part, on completion and on the manifest, 503 forever;
- connection dropped mid-body twice, closed with no answer twice, dropped mid-part;
- a stall longer than the timeout, an answer slower than the timeout;
- blaze killed mid-fetch, what it kept aside still there after a collection an hour
  later, the next run fetching only the rest, a stale temporary file of something else;
- collecting a remote and a daemon that stays, as listed in their sections;
- two processes fetching into one store; twelve packs with three requests at a time;
- the server killed mid-fetch and restarted, killed and left dead, killed mid-push, and
  each of those recovered afterwards with no unfinished upload left in the bucket;
- the poisoning and signing cases listed under Trust;
- lazy: nothing compiled, same output, the unused pack not fetched.

    cd harness && BLAZE_BIN=../target/release/blaze-engine python3 remote.py

Takes about eight minutes with both servers. Results of this round are under Regression
gate. The rounds before had 111 and then 131 checks.

One check is conditional. MinIO reads again 1.3 s after a kill and a fetch always rides
that out. SeaweedFS took between 14 and 23 s to read again in these runs, on either side of
the 16 s blaze waits for a byte. So the harness times how long the server was away: under
10 s the fetch has to succeed with at least one resume; over that it may either succeed, or
fail with no pack in the store and succeed when run again. Before that was written down the
check failed on SeaweedFS in two runs of eight.

Against a real bucket: `BLAZE_S3_REMOTE=s3://bucket/prefix python3 remote.py`, with
credentials wherever the AWS tools would find them. That runs the round trip, the
configuration, poisoning, signing and lazy checks, and skips the ones that need the proxy
or need to kill the server. **Not run.**

## Regression gate

Run on Linux x86-64 on the code as pushed (`blaze-collate` at `1547078a30` merged in),
with the `--features llvm` binary (LLVM 20.1) under `BLAZE_BACKEND=cranelift` and under
`BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`. Both gave the same results:

| Script | Result |
| --- | --- |
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 43 of 43, 5 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 58 of 58 |
| `keys.py` | 36 of 36, 0 open |
| `debuginfo.py` | all right |
| `buildmodel.py` | 180 of 180 as expected, 0 WRONG |
| `storekills.py --kills 100` | PASS, 100 kills |
| `storecorrupts.py --rounds 4` | PASS, 4 damaged stores |
| `storefull.py` | PASS, 6 full disks |
| `storeshared.py` | PASS, 180 builds |
| `remote.py` | 178/178 |

`breakers.py` has 5 open on the base branch as merged; the script exits 0 with them.
`buildmodel.py` does both backends itself and was run once. The `cross_aarch64` row that
failed last round passes with the fix on `blaze-collate`.

`remote.py` was run twice under Cranelift with the Cranelift-only binary (7m38s, 7m45s)
and once under LLVM (8m17s): 178/178 each time. While the round was being written three
checks failed once each and were real: the pull on a changed manifest (the daemon's own
push had taken the other machine's lines for its own), the certificate that was still
waited out for 26 s (the cause sits inside two transport errors, not in their source
chain), and one run in which the disk of this machine was full.

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine`: 512, 24, 330, 33 and 56 passed, 0 failed.

`blaze-collate` has moved to `526f2963ba` since this round started; that is not merged in.

### CI

The Linux job of `.github/workflows/blaze.yaml` now runs `remote.py` after the store
harnesses, under both backends. The runner can start MinIO without privileges: MinIO is one
statically linked program, and the job takes it out of the `cgr.dev/chainguard/minio` image
(pinned by digest) with `docker create` and `docker cp`, which the runner's user may do
without sudo. The old ways to get it are gone: `minio/minio` on Docker Hub and the download
on dl.min.io both refuse. SeaweedFS is not in CI; the harness says "left out" and runs the
MinIO leg. The harness now packs and unpacks objects with the `zstd` command; the job was
not changed for it and the run of this push was not waited for.

The CI run of the round before (run 39040145198): the MinIO step passed, and `remote.py`
gave 67/67 under both backends on the runner. The two harness jobs are red all the same,
for two things that are red on `blaze-collate-wip`'s own run (39031491164) too:
`debuginfo.py` ("wrong: 4" under both backends on the runner; it passes here) and
`buildmodel.py` (the `cross_aarch64` row). Everything else in both jobs passed, and `fmt and tests`
is green.

## What is still open

- **Real AWS has not been run.** The SDK is the same code against AWS as against MinIO, but
  virtual-hosted addressing against real DNS, IAM denial messages, and role credentials
  (web identity, ECS, EC2) are untested. Needs credentials.
- **The machines were directories, not containers,** and the network was loopback. Transfer
  times are best cases, and what compression is worth in time was not measured.
- **Six seconds of the slowdown on cargo are not explained.** They are not debug info and
  not the store; see above.
- **Lazy hydration fetches whole packs and does not win on a fresh machine.** It is behind
  `BLAZE_REMOTE_LAZY`. Fetching a pack ahead of the thread that will want it, or packs
  cut by crate so that a build wants fewer of them, were not tried.
- **A machine with an empty store can be given any list that was ever signed.** The turn
  keeps out a list older than one a machine has taken, and a machine that has taken none
  has nothing to compare with. See `SECURITY.md`.
- **The lazy hook is one per process** (`fetch_missing_with` in `store.rs`, first call
  wins). The daemon's pull on a miss hangs on the same hook. That fits a daemon with one
  remote; it would have to become part of the store if a daemon ever serves two.
- **A remote collection is safe by the clock.** A push that takes over an hour between
  sending a pack and listing it, or a clock that is hours off, is outside what it
  promises. A lock on the remote would close that; S3 has none that every compatible
  service implements, which is why it is the clock.
- **Merged code is held twice until someone collects.** A push still sends a merged pack
  whose keys are all listed already. `cache gc --remote` takes the smaller packs off the
  list; nothing runs it on its own.
- **The keys of a store are read from the pack headers, not from an index file.** Telling
  a merged pack by its keys used to read and checksum every local pack; it now maps each
  pack and walks the record headers only, with no value read and no CRC. A file of keys
  beside each pack would make it one read, but the store's directory holds only what the
  store knows, and adding a kind of file to it is the disk-cache branch's to decide. The
  walk was tested for giving the same keys as the full read; it was not timed on a store
  the size of cargo's.
- **A part of a fetch kept aside lasts a day.** A collection used to remove it a minute
  after its last byte. Now `*.fetch.tmp` and `*.zst.tmp` are kept for a day and counted by
  `cache stats` as temporary bytes meanwhile; they do not count against `BLAZE_CACHE_MAX`.
- **One failed pack stops the rest.** When a fetch runs out of resumes the remote is taken
  to be down and the other transfers of that process stop rather than each waiting out its
  own 16 s. The next run resumes them, and the daemon's next push or look tries afresh.
- **A daemon killed with SIGKILL pushes nothing on its way out.** The next push sends it.
- **Windows and macOS were not run.** The partial-file lock and the rename are std calls
  that exist there; the harness is Linux only, and the daemon's signal path on Windows is
  untested.
