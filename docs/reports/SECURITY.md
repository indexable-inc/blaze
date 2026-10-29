---
status: record
summary: Security review of the store, the remote and the daemon's boundaries.
date: 2026-10-20
---
# Security review of the store, the remote and the daemon's boundaries

Branch `blaze-security`, on top of `blaze-collate` at ce52276cf5, which has the S3 remote
with its signed lists, compressed packs and `cache gc --remote`, and the platform ABI merge. Everything below was
measured on Linux x86-64 with Rust 1.99.0 and LLVM 20. Where something is an argument and
not a measurement it says so. The Windows code in this round was compiled for Windows and
never run; that is said again where it matters.

## Threat model

blaze compiles a user's code and keeps the result, on that user's disk and optionally on
a remote that other machines read. What it keeps is machine code that later builds link
and run. So the question for every party is the same: can they decide what ends up in a
program the user builds, and can they read what the user built?

### Who is trusted

**The user.** blaze runs as the user and can do nothing the user cannot. Every process
running as the same user is inside the boundary: it can write the store, the `target`
directory and the sources, and talk to the daemon. blaze does not defend against it and
does not claim to. In particular there is no isolation between two checkouts of the same
user: one can find what the other stored, by design.

**The user's checkouts, with their build scripts and proc macros.** A build script and a
proc macro are programs that the checkout, or a crate it depends on, has cargo compile and
run during a build. They run with the user's full rights under cargo, and they run with
exactly the same rights under blaze. blaze adds no sandbox and removes none. Building a
checkout with blaze is as much an act of trust in it as building it with cargo, and no
more. What blaze guarantees is narrower and is checked (see "Build scripts and proc
macros" below): it does not run a build script or a proc macro that cargo, given the same
selection of packages, features, profile and target, would not have run. It also does
not run them with different arguments, a different environment or a different working
directory: either cargo itself runs them, or the daemon repeats cargo's own run from a
record of it.

**The signer of a remote.** A machine that pulls names the public keys it trusts
(`BLAZE_REMOTE_TRUST`). A holder of one of those keys can make every
machine that trusts it run any code, by signing a list that names a pack of their making.
That is what the trust means. A signer vouches that its packs were compiled by an honest
blaze from the sources their keys name, on a machine whose store nobody else could write
to. blaze guarantees that nothing reaches a build from a signed remote unless a trusted
key signed the list naming it, for the project it was signed for, and that a machine
never goes back to an older list than one it has already taken. A machine that has taken
none can be given any list a trusted key ever signed; see "Rollback protection for signed
lists".

### Who is not trusted

**Other users of the machine.** They must not be able to change what the user's builds
run, nor read what the user built. blaze guarantees: the store directory is created for
the user alone (0700, files 0600; on Windows it has the access list of the directory it
is made in), and a
store or a file in it that belongs to someone else, or that someone else could write to,
is refused with the reason and the build goes on without it. On Unix this is the owner and
the mode, read from the open handle. On Windows it is the owner and every entry of the
access control list, written this round and not yet run. The daemon's socket is in a 0700
directory, is 0600, and the daemon checks that the peer is the same user (that code is
`crates/blaze-engine/src/socket.rs`, reviewed and not changed here). What blaze does not
guarantee: that timing tells another user nothing (they cannot reach the daemon or the
store, so they would have to infer it from machine load), and anything at all against
root or, on Windows, the system and the administrators.

**An unsigned remote, and anyone who can write to a remote without a trusted key.** They
decide the bytes of every object and every list. blaze guarantees:

- Nothing is pulled from a remote whose lists nobody trusted signed, unless the user sets
  `BLAZE_REMOTE_UNSIGNED=1`, which is the user saying that everyone who can write there
  is trusted as the signer would be.
- With signed lists, a writer without a key can delete or withhold, which costs cache
  hits. It cannot add or change, because every pack is named by the SHA-256 of its bytes
  and every key inside it by the full content hash of what it was compiled from. It can
  put back an older signed list only for a machine that has not yet taken a newer one.
- No answer of a remote is read past a bound, and no pull writes more than a bound,
  whatever the remote sends (see "Bounds on what a remote sends").
- No bytes from a remote make blaze panic, index out of range or write outside the place
  they belong: the parsers are fuzzed and dependency archives are unpacked only as plain
  files and directories of their own unit.

**Readers of a remote.** Whoever can read a remote learns what was pushed: machine code,
names of functions and crates, source lines in debug info. blaze guarantees that the
paths of the pusher's machine are not among it: files of the checkout are named relative
to it, the standard library's as `/rustc/<commit>/...`, and, from this round, files cargo
fetched as `/cargo/registry/...` and `/cargo/git/...`. `storeguards.py` looks through
every byte of a store and of the remote it was pushed to for the checkout, the home
directory and the cargo home.

**A damaged disk.** Bytes that are not what was written: a torn write, a truncated file,
flipped bits, a file from another version. blaze guarantees that damaged bytes are
detected (SHA-256 per pack on first read, CRC per record on every read, a format version
in the store and in each pack), discarded and recompiled, never linked. Stored entries
that pass the checksum are still validated structurally before use. A disk that is
damaged by someone who also recomputes the checksums is not damage, it is a writer, and
is covered by the paragraphs above.

### What follows from it

The local store is as sensitive as `target/`: whoever writes it owns the user's builds.
The remote is as sensitive as its signing keys. Everything else, including the content of
any file blaze reads from a store or a remote, is treated as hostile input.

## This round

Each with its test. The findings list below has them under the numbers given here.

| | What | Where it is proved |
|---|---|---|
| H3 | A machine that names no keys pulls nothing unless told to, now on the S3 remote | `signed::tests::a_list_nobody_signed_is_taken_only_when_asked_for`; `remote.py`, `storeguards.py`, `deps.py`, `storefull.py` |
| H5 | A signed list that was put back is not taken again | eight tests in `signed::tests` and `remote::tests`; `remote.py` |
| M6 | Files cargo fetched are stored under `/cargo`, not the pusher's cargo home | `cached::tests::a_fetched_file_is_stored_under_the_same_name_from_every_cargo_home` and two more; `storeguards.py` |
| M7 | Every read from a remote is bounded, and so is what a pull writes | eleven tests in `remote::tests` and `deps::tests`; fuzz loops; `remote.py` |
| M8 | blaze ran build scripts of dev-dependencies that cargo would not have | `harness/selects.py` |
| L6 | Windows: owner and access list checked | compiled for `x86_64-pc-windows-msvc`; three tests for the Windows session |

## Findings, by severity

### High

**H1. Crate identity was 64 bits. Fixed.** `Contents::id` and `Contents::library` kept
eight bytes of a SHA-256 as the id of a crate's content, and that id is the crate part of
every stable function name, which is what store keys are hashed from. On a shared remote
that is a birthday bound of about 2^32 crates for an accidental wrong-code hit, and a
chosen collision is within reach of someone who wants to plant code for a crate they do
not control. The id is now `ContentId`, all 32 bytes, printed in full into the name. The
hashes that feed it were also changed to hash their dependencies' full ids, and the entry
key now length-prefixes each field (`blaze entry 6`) so that no two field lists are the
same bytes. Test: `cached::tests::a_crate_is_named_by_all_of_its_hash_and_a_key_by_all_of_a_name`
builds two ids that agree in every byte the old id kept and checks the names and keys
differ, and that debug, overflow checks and a trailing byte each change the key.

**H2. A store that others can write to was used. Fixed.** Nothing checked who owned the
store or what its mode was, and files were created with the process umask (typically
0644 in a 0755 directory). Now: created 0700/0600; an existing directory that is readable
by others is closed to them; one that is another user's or group/world-writable is refused
by the daemon (`store: not used`, with the reason, and the build succeeds uncached) and by
`blaze-engine cache stats`, `gc` and `verify`. Files are checked with `fstat` on the handle that
is then read, so there is no window between check and use. `blaze-engine cache verify` lists a
pack others can write to as corrupt; a daemon removes it. Tests in `store::tests`:
`the_store_and_its_files_are_the_users_alone`, `a_store_others_could_read_is_closed_to_them`,
`a_store_others_can_write_to_is_refused`, `a_pack_others_can_write_to_is_not_read`,
`a_store_that_is_no_directory_is_refused`, `a_store_of_another_user_is_refused` (this one
only does anything as root; I ran it with `sudo` and it passed). End to end:
`harness/storeguards.py`.

**H3. Unsigned remotes were pulled from without asking. Fixed.** A machine that names no
key in `BLAZE_REMOTE_TRUST` takes no list from a remote, and so fetches nothing, unless
`BLAZE_REMOTE_UNSIGNED=1`. Only the exact value `1` counts, and it does not undo naming
keys. The refusal is in `Trust::open` (`crates/blaze-engine/src/remote/signed.rs`), which every
list a pull reads goes through: `blaze-engine cache pull`, `blaze-engine deps pull`, the daemon's pull
before a build and its lazy fetching. Pushing asks for nothing; it reads the list it adds
to through `open_to_extend`, which takes nothing from it. Tests:
`signed::tests::a_list_nobody_signed_is_taken_only_when_asked_for`, `harness/remote.py`
(refused with empty, `0` and `true`, nothing fetched; taken with `1`; a push with nothing
set goes through), `harness/storeguards.py`, `harness/deps.py`, `harness/storefull.py`.

**H4. A dependency archive could write outside its unit. Fixed.** `blaze-engine deps pull`
unpacked tar archives from the remote. Entries were only checked by path prefix, so a
symlink entry followed by a file through it, or a hard link, could place files elsewhere,
and the boolean `unpack_in` returns for a skipped `..` path was ignored. Now only plain
files and directories of the unit are accepted and a skipped entry is an error. Test:
`deps::tests::a_unit_with_a_link_or_a_file_of_another_is_refused`.

**H5. A signed list could be put back. Fixed.** A signature said who wrote a list and
not when, so a remote, or anyone who can write to it, could serve a list the signer had
since replaced, for ever, including one naming a pack the signer took out. Signed lists
now carry a turn and a machine remembers the highest it took. See "Rollback protection
for signed lists".

### Medium

**M1. Stored bytes were trusted once the checksum matched. Fixed.** An entry that decodes
is not necessarily one that is safe to use: relocations could point past the end of the
code or the data, or name functions and data the entry does not have, and the code that
applies them indexes without checking. `Entry::decode` and `StoredCode` decoding now
validate every offset and index (`sound`) and return nothing for an entry that fails, which
is a cache miss and a recompile. Tests: `cached::tests::an_entry_that_names_what_it_lacks_is_no_entry`,
`code_of_llvm_that_names_what_was_not_asked_for_is_not_restored`.

**M2. A stored debug type could overflow the stack. Fixed.** `DebugType` is recursive and
postcard will follow nesting as deep as the bytes say. Decoding now stops at 128 levels,
and types deeper than that are not written either. Test:
`cached::tests::a_type_inside_too_many_others_is_not_read_and_not_kept`.

**M3. The pack reader added a length from the file without checking. Fixed.** `start + len`
is now a checked add followed by a checked slice. On 64-bit this could not overflow with a
32-bit length, on a 32-bit host it could. Covered by the pack fuzz loop.

**M4. Standard-library paths carried the user's home directory into the store and so into
a remote. Fixed.** This fell out of `storeguards.py`. Names of standard-library items are
meant to use `/rustc/<commit>/...` as rustc does, but the commit was read from a field of
rustup's manifest that 1.99.0 does not have, so the names silently fell back to
`/home/<user>/.rustup/toolchains/...`. Two effects: every pack held the user's home path
thousands of times, and two users never shared a standard-library instantiation. The
commit is now also read from the built `libstd` rlib. Tests:
`tests::the_commit_of_a_toolchain_is_the_one_its_library_has_its_sources_under` and
`storeguards.py`, which greps every byte of the store and of a pushed remote for the
checkout path and the home directory (1.6 MB each, 0 occurrences).

**M5. A file-name parser accepted names it should not. Fixed.** Found by the fuzz loop:
a name like `<hash>.pack.4242\0-17.tmp` was taken for one of the store's temporary files
(and so could be removed by collection). The writer suffix must now be digits and hyphens.
Test: `store::tests::no_name_breaks_the_telling_of_what_a_file_is`.

**M6. Registry and git paths carried the pusher's cargo home. Fixed.** A crate cargo
fetched has its sources under `$CARGO_HOME/registry/src/<index>/<name>-<version>/` or
`$CARGO_HOME/git/checkouts/...`, and that absolute path was in the stable names of its
generic instantiations, in panic locations, in `file!()` and in debug info. So packs held
the pusher's home directory, and two users with different cargo homes shared none of
that code. The cargo home is now found from the crates themselves (the outermost
`registry/src` or `git/checkouts` above a crate root outside the checkout; nothing is
read from the environment, so it is what cargo actually used), and:

- stable names, and so store keys, are made from the path with `/cargo` in place of the
  home (`StableNames::stored`);
- the content id of an outside crate hashes its root the same way (`blaze library 4`);
- an entry on its way to the store has every path it carries rewritten (`Entry::leave`):
  panic locations, `file!()` strings and the file table of its debug info;
- an entry on its way out of the store has them rewritten to the cargo home of the
  machine that reads it (`Entry::arrived`), so a panic message and a debugger show a file
  that exists there, as they do under cargo.

Three cases are refused rather than guessed. A constant whose bytes contain the cargo
home (a string built from `file!()` at compile time, say) is not stored. A path that
already starts with `/cargo/` when the cargo home is somewhere else is not stored, since
it would be read back as a fetched file. An entry with fetched files is a miss on a
machine where no cargo home can be told. If crates come from more than one cargo home
nothing is renamed and those paths stay as they are, which the path checks then keep
local as before.

The code compiled into a function does not contain the path: locations and `file!()`
strings are data the entry refers to, which is why rewriting them is enough. That is
measured, not assumed: `storeguards.py` builds a program on a registry crate that uses
`file!()`, a panic with a location and a generic function, under one cargo home, then
again under another with the same store. The second build finds every entry the first
kept (302 of 302, both backends) and adds none, the program prints the path under its own
cargo home exactly as cargo's build does, and neither home is anywhere in the store or in
the remote the store is pushed to, while `/cargo/registry/src/` is. Unit tests:
`cached::tests::a_fetched_file_is_stored_under_the_same_name_from_every_cargo_home`,
`a_fetched_file_is_not_taken_where_cargo_has_no_directory`,
`a_directory_named_as_cargos_stands_in_is_not_mistaken_for_it`,
`content::tests::the_cargo_home_is_told_from_a_file_cargo_fetched`.

What is not remapped: the index directory name (`index.crates.io-<hash>`) stays, and is
the same for everyone using the same registry; a private registry's name is in it. Paths
of path-dependencies outside the checkout and outside any cargo home are absolute, and
code that names them stays local as before.

**M7. A remote decided how much was read and written. Fixed.** Lists and key lists were
read whole into memory whatever their size, and a pack or an archive was written to disk
for as long as the remote kept sending. See "Bounds on what a remote sends".

**M8. blaze ran build scripts cargo would not have. Fixed.** See "Build scripts and proc
macros".

### Low, not changed

**L1. rustc's `StableCrateId` is 64 bits and names crates from outside the checkout.**
Registry crates and the standard library are named by rustc's own id, which is a 64-bit
SipHash with a public key. I left it, with this argument: the id is never all that tells
two crates apart. Every item name also has the path of its source file, which for a
registry crate ends in `registry/src/<index>/<name>-<version>/...`, and for the standard
library is `/rustc/<commit>/...`. To get code looked up under serde's names an attacker
needs a crate with serde's id *and* its files at serde's paths, and whoever can put files
there already owns the build. Inside the hashes of checkout crates the id is mixed in next
to the name and version, under SHA-256. This is an argument, not a test.

**L2. `own_crate_id` is 64 bits of a SHA-256.** It stands for a checkout crate that rustc
built nothing of, in two places: linker symbol names (`blaze.<id>.<path>`) and `TypeId`s,
where it sits in the slot rustc's 64-bit id would. Neither decides which stored code is
used: stable names of checkout crates use the full `ContentId`. A collision needs two
crates of one checkout with the same name hash and would be a duplicate symbol at link
time. Left at 64 bits because `TypeId` has to match rustc's layout.

**L3. The other non-cryptographic hashes do not cross a process.** `FxHasher` in
`inputs.rs` (file stamps), `main.rs` (settled-files check), `outline.rs` and
`codegen/patch.rs` (what changed since the last patch) all live and die inside one daemon
and only decide whether to redo work. `type_id.rs` uses rustc's 128-bit stable SipHash
because it has to produce the same `TypeId` rustc does. The CRC32 per record is an error
check behind the pack's SHA-256. Store keys, pack names, the LLVM code key and the
assembled-asm key are SHA-256.

**L4. The asm key does not length-prefix.** `asm::assemble` hashes `triple` then `text`
with nothing between. No supported triple is a prefix of another followed by valid
assembly, so I do not see a collision, but it should be made unambiguous the next time
that key changes.

**L5. A symlink in the store is followed.** Only the user can create one there, and the
target is checked with `fstat` like any file, so it has to be the user's and not writable
by others.

**L6. Windows had no owner or access check. Written, not run.** See "Windows".

## What an attacker can make a build run

With write access to the local store directory: anything. They compute the key of any
function (names are derivable from public sources), write a pack with their code under it,
and the next build links it. This is by design the same as write access to `target/`. The
fix is H2: blaze refuses to be in that position.

With write access to an unsigned remote, before this round: the same, for every machine
that pulls. After: nothing, unless the user set `BLAZE_REMOTE_UNSIGNED=1`.

With write access to a signed remote but no trusted key: they can delete or withhold,
which costs cache hits but never gives wrong code, since every pack is named by its hash
and every key includes the content it was built from. They cannot add. They can put an
older signed list back, and a machine that took a newer one refuses it (H5); a machine
that never pulled takes it.

With read access to a remote: see "What one client can learn" below.

## Parsing untrusted bytes

cargo-fuzz needs nightly, so these are plain loops inside the unit tests. Each takes the
checked-in seeds (`crates/hir-ty/src/mir/codegen/corpus/`, four files, 36 KB on disk with
directories) plus seeds the test builds, and applies one to four mutations: byte flips,
edge values (0, 1, 0x7f, 0x80, 0xff...), truncation, insertion, and splicing of two seeds.
The generator is a fixed xorshift so every run is the same. A test-only global allocator
records the largest single allocation each decode asks for and the test fails if it
exceeds 1 MB plus 64 bytes per input byte, which is how "no unbounded allocation" is
checked rather than assumed.

Default run (20,000 mutations each, part of `cargo test`):

| Target | Accepted | Refused | Panics |
|---|---:|---:|---:|
| `Entry::decode` | 824 | 19,179 | 0 |
| `StoredCode` decode (LLVM build only) | 1,961 | 18,041 | 0 |
| pack reader | 131 whole | 19,873 not | 0 |
| store file names | all classified | | 0 |
| manifest | 20,000 inputs | | 0 |
| dependency key list | 20,000 inputs | | 0 |

Long run, `BLAZE_FUZZ_RUNS=2000000 cargo test --release -p hir-ty --features llvm --lib no_`:
entry 83,573 accepted and 1,916,430 refused; stored code 193,184 and 1,806,818; pack
11,759 and 1,988,245; 0 panics, 0 allocations over the bound, 4.7 seconds. Every accepted
entry is then walked the way the linker walks it (`follow`) to show that no offset or index
in it is out of range.

What fell out: M1, M2, M3 and M5 above. Accepted manifest lines are checked to be exactly
64 hex digits and `.pack`, with no path separator.

The remote-facing parsers have the same loops, in `crates/blaze-engine`, with the same mutator:

| Target | Test | Inputs | What is asserted of every input |
|---|---|---|---|
| A manifest | `remote::tests::no_bytes_break_the_reading_of_a_manifest` | 20,000 | no panic; every pack and key list it names is 64 hex digits; what is found to fit under a limit sums to no more than it |
| A list of units | `deps::tests::no_bytes_break_the_reading_of_a_list_of_units` | 20,000 | no panic; every unit is a crate name and a hash with no separator, every archive 64 hex digits |
| A signed list | `signed::tests::no_bytes_are_taken_for_a_list_that_was_signed` | 20,000 | no panic; whatever verifies is the body that was signed, at the turn it was signed at |
| An archive of a unit | `deps::tests::no_bytes_of_an_archive_write_outside_of_its_unit_or_past_the_bound` | 3,000 (`BLAZE_FUZZ_RUNS`), each zstd-packed and bare | nothing is written outside `deps/` and `build/` of the target directory, and no more than the 64 KiB bound |

Packs pulled from a remote are read by the pack reader and `Entry::decode`, which are the
first two rows of the first table.

## Bounds on what a remote sends

Before this round a list and the keys of a pack were read whole into memory (`tokio::fs::read`
for a directory, `body.collect()` for S3), and a pack or an archive was appended to a
file for as long as bytes arrived. Whoever writes to a remote, or answers for it, chose
how much. Now:

| What | Bound | Set by |
|---|---|---|
| A manifest, or a list of units | 16 MiB | fixed (`LIST_MAX`); a line is under 200 bytes |
| A pack as it is sent (zstd) | the compressed size its manifest lists | the manifest |
| A pack as it unpacks | the size its manifest lists | the manifest |
| All the packs one pull writes | the store's limit, 8 GiB | `BLAZE_CACHE_MAX`; a pull fetches only what its listed sizes say fits |
| The keys of one pack | the pack's listed size, and 512 MiB | `BLAZE_REMOTE_MAX` |
| All the keys a lazy daemon holds in memory | 512 MiB | `BLAZE_REMOTE_MAX` |
| A pack a lazy daemon fetches | its listed size, and the store's limit | `BLAZE_CACHE_MAX` |
| The archive of one unit | 512 MiB | `BLAZE_REMOTE_MAX` |
| All that one `blaze-engine deps pull` unpacks | 64 GiB | `BLAZE_DEPS_MAX` (`off` for none) |

How each is held:

- Small objects go through `read_at_most` (`remote.rs`), for a directory and for S3
  alike: it takes the body a chunk at a time and stops with an error at the first chunk
  that passes the bound. What the service says the length is, is not consulted.
- A pack or an archive goes to a file through `fetch_file` (`remote/transfer.rs`), which
  now takes the most the object may be and stops before appending the chunk that passes
  it. The part file is removed, the object is counted under "objects rejected", and the
  failure is final: it is not asked for again. A part file left by a stopped process
  that is already longer than the bound is emptied first.
- A pack is sent compressed, and a manifest line gives both sizes. The object is held
  to the compressed one while it arrives, and `unpack_whole` (`remote/transfer.rs`)
  counts what comes out of zstd against the unpacked one and stops at the read that
  passes it, removing what it wrote. So a small object that unpacks to a great deal
  writes no more than the pack was listed as: in the test 64 MiB of one byte are under
  64 KiB as sent, and listed as 0, 64 KiB and one byte short they leave nothing in the
  store.
- A pack's bound is the size in its manifest line, which with signed lists is a number
  the signer wrote. The existing `fitting` already left out packs whose listed sizes do
  not fit under `BLAZE_CACHE_MAX`; with each pack held to its listed size, that sum is
  now a bound on what is written and not a hope.
- The keys of a pack are 32 of every 40 or more bytes of it, so their list is never
  longer than the pack.
- Unpacking counts each tar entry's declared size plus its header before the entry is
  unpacked, which is what stops a small archive that expands: the test packs 64 files of
  1 MiB of zeros into an archive under 64 KiB; a pull held to 4 MiB writes at most 4 MiB
  and fails, and held to exactly 64 MiB plus headers it succeeds.

Tests, each with the exact bound taken and less refused:
`remote::tests::an_object_is_read_to_the_bound_and_not_past_it`,
`an_object_without_an_end_is_given_up_on` (`/dev/zero` as the object),
`a_pack_longer_than_its_list_says_is_not_written` (listed as 0, 8, 64 KiB and one byte
short: rejected, the store holds nothing but its own two files; listed exactly: fetched),
`a_pack_that_unpacks_to_more_than_its_list_says_is_not_written` (the zstd bomb),
`a_packed_pack_longer_than_its_list_says_is_not_unpacked`,
`what_a_stopped_fetch_left_does_not_count_against_the_bound_twice`,
`a_manifest_longer_than_is_read_is_refused` (16 MiB read; one byte more refused for a
pull and for a push, and the list not overwritten),
`the_keys_of_a_pack_are_no_longer_than_the_pack`, and in `deps::tests`
`an_archive_longer_than_is_taken_is_refused`,
`a_list_of_units_longer_than_is_read_is_refused` and
`an_archive_that_unpacks_to_more_than_is_taken_is_refused`. Against MinIO, `remote.py`
does, through `blaze-engine cache pull`: a pack listed as 0, 1 and one byte short unpacked, and
as 0 and one byte short as sent; 2,115 bytes that `blaze-engine cache push` made of a 64 MiB
pack, listed as 1 MiB; and the manifest of 16 MiB and of one byte more.

Not covered: with `BLAZE_CACHE_MAX=off` the sum of what a pull writes has no bound, which
is what the setting says. The lazy path's two bounds have no test of their own.

## What was applied on top of the S3 remote

`blaze-collate` has `blaze-s3`, in two merges while this round ran (526f2963ba, then
2da3d6a792 with compressed packs, `cache gc --remote` and the daemon that stays). The
first replaced `remote.rs` and
`deps.rs` wholesale (the remote is now asynchronous, streams packs to files and has
`remote/s3.rs`, `remote/signed.rs`, `remote/transfer.rs`), so what this branch had done
to the old remote was done again on the new one, not merged:

- **The unsigned-pull refusal** is in `Trust::open`, as described under H3. The
  `Unsigned` type the old remote had here is gone.
- **The bounds** are as in the section above.
- **The archive checks of H4** (only files and directories, the result of `unpack_in`
  looked at) are in the new `unpack`, which reads the archive from the file it was
  fetched to.
- **Rollback protection** is the next section.
- **A fetched pack is made 0600.** The S3 remote wrote a pack it fetched with the
  process's default mode (0644 here), into a store whose every other file is 0600.
  `storeguards.py` found it after the merge; the part file is now opened 0600
  (`remote::tests::a_pack_longer_than_its_list_says_is_not_written` checks the mode of
  what arrives).
- **A machine that signs takes only signed lists,** also when it names no keys to trust
  and has `BLAZE_REMOTE_UNSIGNED=1`: what it pulls it later pushes under its own
  signature, so an unsigned list would be laundered into a signed one.
- **`Kind::of` in `store.rs`** takes `<pack>.fetch.tmp` for a temporary file of the
  store. The stricter names of M5 did not know the name S3 gives a pack on its way, so
  the store neither counted one nor removed it once it was old; `remote.py` found it.
- **`cache gc --remote` and a signed list.** Collecting rewrites manifests, so it was
  checked for a way round the signature. What was already right: it reads a manifest
  through `read_list`, so a machine that does not trust a list leaves its lines alone,
  and it removes only objects that no list names, reading every list for that whoever
  signed it. What was wrong, and older than `gc`: a machine with no key and no keys to
  trust that wrote a list back (a push, or a collection with
  `BLAZE_REMOTE_UNSIGNED=1`) wrote it without a signature, over a signed one. Every
  machine that trusts the signer then takes nothing, and the signer's next push starts
  the list again from what it sends. That is a writer to the remote doing through
  blaze what it could do with the bucket's own tools, so no new power, but blaze should
  not do it by accident. Now `rewrite_list` refuses: "is signed, and this machine has no
  key in BLAZE_REMOTE_SIGN: it is left as it is". A collection counts such a manifest
  with the ones it does not trust and goes on. A signer that collects signs what is
  left at a later turn, and does not shrink a list that was put back under it. The
  bounds hold in `gc` too: lists to 16 MiB, key lists to their pack's size. Tests:
  `remote::tests::a_machine_without_a_key_does_not_write_a_signed_list_back`; in
  `remote.py`, `gc --remote --older-than 0s` by a keyless machine with and without the
  unsigned setting leaves the list and every object as they were, its push is refused,
  and the signer's collection is signed at a later turn and taken by the reader.
  `gc --remote` without `BLAZE_CACHE` remembers the signer's turn in the daemon's home.
- **Temporary names.** `Kind::of` also takes `<pack>.zst.tmp` and `<pack>.send.tmp`,
  the two names the compressed transfer leaves, for the store's.
- **`harness/remote.py`** runs its machines with `BLAZE_REMOTE_UNSIGNED=1` where they
  name no keys, and has a new section, "unsigned lists, old lists, long objects",
  25 checks. `harness/deps.py` expects the new remote's wording for a changed archive
  ("rejected units/...").

## Rollback protection for signed lists

Designed before the signed lists were on this branch, and applied when they arrived. The
code is in `crates/blaze-engine/src/remote/signed.rs` (`Turns`, and the turn in `seal` and
`verify`) and in `Remote::read_list` and `Remote::extend_list` in `remote.rs`.

**What it is for.** A list is signed, so a remote cannot invent one. It could serve an
old one. Every pack an old list names was vouched for once, so this never gave code
nobody signed. It matters when a signer has taken something back: a pack built by a
blaze with a miscompilation, or pushed from a machine later found to be compromised,
removed from the list by signing a new one. Without this the remote, or anyone who can
write to it, could keep handing out the old list for ever.

**The signed body.** Every signed list has a number, its turn. The last line is
`signed <key> <turn> <signature>`, and the signature is over
`blaze list 2\n<name>\n<turn>\n<body>`. The domain line was `blaze list 1`, so a
signature of the old kind verifies under no key, and a list signed without a turn is
"not signed". The turn is a decimal `u64` written the one way there is to write it (no
sign, no leading zero); anything else is "not signed". It is inside the signed bytes, so
changing it in the line breaks the signature.

**Who sets it.** A machine that pushes reads the list it extends and signs with
`turn = max(highest turn it remembers of this list + 1, seconds since 1970 by its
clock)`. A counter alone would order the lists. Starting it from the clock is so that a
remote that was emptied and started again, or a list signed by a second key on a machine
that never saw the first, still comes out above what readers remember, with no state to
keep on the signing side beyond what every machine keeps. A signer whose clock is far
ahead only makes the numbers large; the next goes one above it. The readers' clocks are
never consulted, so a wrong clock on a build machine cannot make it refuse or accept
anything.

I chose a counter seeded from the clock over a plain timestamp because two pushes within
one second must still be ordered, and over a plain counter because of the reset case.

**What a machine remembers.** For each list it has taken, the highest turn, in a file
`turns/<sha256 of the remote>/<sha256 of the list's name>` holding the decimal number
and a newline. It is written like every file of the store (temporary file, fsync,
rename, directory fsync), in a directory made 0700. Where:

- For `blaze-engine cache` and the daemon, in the store (`BLAZE_CACHE`). The store is what the
  list fills, it is already private to the user and checked for that, and removing the
  store forgets the turns together with the packs they protected. The store's own code
  leaves a name it does not know alone, which `turns` is.
- For `blaze-engine deps`, in the store when `BLAZE_CACHE` is set, and otherwise in `.blaze/`
  of the target directory the units are unpacked into. `cargo clean` forgets them with
  the units.

The remote is named by its directory as given, or `s3://bucket/prefix` without the
options of the address, so the same bucket reached through another endpoint or profile
is the same remote. The turn belongs to the list and not to a key, so rotating keys
changes nothing: a new trusted key that signs a lower turn is refused like any other.

**The rule,** applied after the signature has verified and before anything the list
names is fetched, under a lock file in the remote's directory of turns so that two
processes cannot interleave a read and a write:

| The list's turn against the remembered one | What happens |
|---|---|
| Nothing remembered (first pull, or the store was removed) | Taken, and remembered. |
| Higher | Taken, and remembered. |
| Equal | Taken. Two machines that push at once can sign the same turn over different lines; both are signed by trusted keys, and a remote choosing between them gains nothing it could not by withholding. |
| Lower | Refused: "`manifests/default` is turn N, and this machine has taken turn M of it: an older list is not taken". Nothing is fetched, and the list is counted as rejected. |
| Signed without a turn, or under the old domain | Refused as not signed. There is no released version to stay compatible with. |
| The remembered file cannot be read, or does not hold a turn | Refused, naming the file and `blaze-engine cache forget REMOTE`. It is not taken for nothing remembered: what keeps an old list out must not be something damage removes quietly. The file is left as it is. |
| The turn cannot be written (full disk, read-only store) | The pull fails before fetching. |

`blaze-engine cache forget REMOTE` (new) removes what is remembered of one remote. It is for a
damaged memory, and for a remote that was deliberately started again by a signer whose
clock is behind the old lists.

**The pushing side.** A signer applies the same rule to the list it is about to extend,
so it does not put its name under a list that was rolled back. Such a list gets what an
untrusted list already got: "it is written anew", with only what this push sent, at a
turn above everything the signer remembers. After a write goes through, the signer
remembers its own turn. A damaged memory stops a push as it stops a pull, and the list
on the remote is not touched. A machine with `BLAZE_REMOTE_UNSIGNED=1` and no keys has
no turns and no protection; that is the meaning of the setting. The lists of `blaze
deps` go through the same code.

**What it does not do.** A machine that has never pulled remembers nothing, so it can be
given any list that was ever signed; this is trust on first use, and the harness shows
it. A remote can also freeze a machine at the newest list that machine has seen, by
never showing a newer one. Both are the same gap, freshness, and need an expiry to
close. The turn makes one possible without another field: a turn is never below the
signing time, so a list whose turn is older than a chosen age was certainly signed
before it. An optional `BLAZE_REMOTE_FRESH=<duration>`, refusing lists with
`turn < now - duration`, would close it for machines that start from an empty store
(CI), at the price of a signer having to re-sign at least that often and of the reader's
clock mattering. It is not implemented, and I would not make it the default.

Also not covered: a remembered turn is not itself protected against the user's own
processes (they are trusted), and an attacker who can roll the store directory back as
a whole (a restored backup, a snapshot) rolls the memory back with it.

**Tests.** In `signed::tests`: `the_turn_is_signed_with_the_list` (a turn edited in the
line fails; six ways to write a turn that are not the one way; `u64::MAX` is taken),
`a_list_signed_without_a_turn_is_not_signed` (the old line, and the old signature given
a turn), `a_turn_is_past_the_last_and_not_behind_the_clock`,
`an_older_list_is_not_taken_again` (first, higher, equal, lower; another list and
another remote have their own; the next process remembers),
`a_memory_that_is_damaged_takes_no_list_until_it_is_forgotten` (six kinds of damage,
each refused for a lower, an equal and a higher turn, the file left as it was),
`two_that_take_at_once_remember_the_later` (eight threads, 400 turns),
`no_bytes_are_taken_for_a_list_that_was_signed` (the fuzz loop). In `remote::tests`,
through `Remote` on a directory with one machine that signs and one that trusts it:
`a_list_that_was_put_back_is_not_taken`,
`a_signer_does_not_add_to_a_list_that_was_put_back`,
`a_memory_that_is_damaged_stops_a_push_and_a_pull`. In `harness/remote.py`, against
MinIO, through the `blaze` binary: A signs and pushes twice, C takes both, the first
manifest is put back in the bucket and C's pull is refused with nothing fetched; a
machine that never pulled takes it; `cache forget` has C take it; A, pushing over it,
writes a new list with a later turn; C takes that and then refuses the second list.

## Build scripts and proc macros

They are the checkout's programs and run as the user, under blaze exactly as under cargo;
blaze neither confines them nor gives them anything cargo would not. The thing to check
was that blaze never runs one cargo would not have. I read every place blaze starts a
process (`grep -n "Command::new" crates/blaze-engine/src`) and every place a build script or a
proc macro can come to run. There are three.

1. **`blaze DIR ...` loading a workspace** (`main.rs`, through `project-model`). blaze
   does not run build scripts or compile proc macros itself here. It has cargo do it:
   `cargo check --compile-time-deps` with the profile, packages, features and target the
   build was asked for. Proc macros are then expanded by the toolchain's own proc-macro
   server, loading the libraries that cargo run built, and only for macro calls in crates
   being compiled.

   **This is where M8 was.** The command also passed `--all-targets` without being asked,
   which is right for an editor and wrong for a build: all targets include tests and
   examples, those bring in dev-dependencies, and cargo then runs the build scripts of
   dev-dependencies and builds their proc macros. Measured on a workspace where every
   script and macro writes a line when it runs: `cargo build -p app` ran one build script
   (of `dep`); the command blaze gave cargo for the same selection ran two (of `dep` and
   of the dev-dependency `devdep`). A crate that is only a dev-dependency is exactly what
   someone building a release binary does not expect to execute. Fixed in
   `project-model`: `--all-targets` is passed with `--compile-time-deps` only when the
   configuration asks for all targets, and blaze asks only when the build has tests in it
   (`--cfg-test` or `--dev-deps`).

2. **`blaze ...`** (`crates/blaze-engine/src/cargo`). Cargo is run as itself with blaze as
   `RUSTC_WORKSPACE_WRAPPER` (`cargo/state.rs`), so cargo decides what is compiled and
   run. The wrapper renames a linked build script to `<script>.real` and leaves a
   two-line shell script in its place that runs `blaze-engine build-script`, which writes down
   the environment and directory cargo gave it and then `exec`s the real script with the
   same arguments. Nothing runs that cargo did not start.

3. **The daemon running a build script again** (`cargo/script.rs`, `rerun_is_same`, called
   from `Guard::broken` in `state.rs`). This is the one place blaze starts a build script
   itself. It does so only for a script cargo has already run in this target directory,
   only when one of the files the script named with `rerun-if-changed` has changed (or
   any file of the package, if it named none), which is when cargo would run it again,
   and with the recorded environment (`env_clear` then exactly what cargo set), directory
   and executable. If its output differs the build is handed back to cargo. A change to
   a `rerun-if-env-changed` variable is never handled this way; that is cargo's.

   The record (`blaze-ran.json`) is in the target directory and names the executable to
   run. Whoever can write there can already replace the script itself, or any rlib, so it
   is no new way in; the target directory is as trusted as the checkout.

The other processes blaze starts are cargo, rustc (as the wrapper, with cargo's
arguments), the linker line cargo's rustc printed, the built program for `blaze
run` and `test`, and itself as daemon or worker.

**The test** is `harness/selects.py`, new and in the workflow. A workspace has an `app`
with a dependency, a dev-dependency and a dev-only proc macro, and another member nothing
depends on; each build script and the macro append a line to a file when they run. For
three selections the same thing is asked of cargo and of blaze, each on a fresh copy, and
every line blaze's build wrote has to be one cargo's wrote:

| Selection | cargo ran | blaze ran |
|---|---|---|
| `build -p app` / `--package app` | script of `dep` | the same 1 |
| `test --no-run -p app` / `--package app --cfg-test` | scripts of `dep`, `devdep`; macro `devmacro` | the same 3 |
| `build --workspace` / `--workspace` | scripts of `dep`, `devdep`, `other` | the same 3 |

Before the fix the first row fails, by the measurement above. Not covered by the harness:
the `blaze` path, where the argument is that cargo is the one running; and a
`blaze DIR` with no package selected at the root of a workspace, which builds every
member as `cargo build` there does.

## Windows

**Why modes are not enough.** Windows has no owner/group/other bits. Rust's
`Permissions` there is the read-only attribute, which says nothing of who. Who may write
a file is its discretionary access control list, mostly inherited from the directory it
was made in. A store under `%LOCALAPPDATA%` inherits a list that lets in the user, SYSTEM
and Administrators. A store at `C:\cache`, or on a second drive, inherits one where
`Authenticated Users` may create and modify files, which is the group/world-writable
store of H2. So the check has to read the owner and walk the list.

**What the code does** (`crates/hir-ty/src/mir/codegen/store/acl.rs`, behind
`cfg(windows)`, called from the same two places as the Unix check: the store directory
when it is opened, each file through the handle it is then read from):

- The owner (`GetSecurityInfo` on the open handle, so there is no window between check
  and use) has to be the user of the process (`OpenProcessToken`, `TokenUser`), SYSTEM or
  the Administrators group. The last two are accepted because they can take any file
  whatever its list says, and because a file made by an elevated process is owned by
  Administrators, not by the user who ran it.
- No list at all means everyone may do everything: refused.
- Every entry of the list is read. Deny entries and inherit-only entries (which apply to
  children, not to the object) are skipped. An allow entry counts if its mask has any
  right that changes the file or directory or who controls it: write or append data
  (add file, add subdirectory), delete, delete child, write DAC, write owner, generic
  write, generic all. Such an entry is fine for the user, SYSTEM, Administrators and
  `OWNER RIGHTS`; for any other identity the store is refused as "can be written to by
  other users", with the `icacls` command that mends it.
- An entry of a kind that is not a plain allow or deny (object, callback or conditional
  entries) with a write right is refused as unreadable rather than interpreted.
- If the token, the owner or the list cannot be read, that is a refusal too.

Reading by others is not refused, on Windows or on Unix; on Unix a readable store
directory is closed to others when it is claimed, and on Windows nothing is tightened:
the store is created with whatever it inherits, and a place that inherits write access
for others is refused with the reason instead of being silently re-permissioned. Setting
an explicit list at creation would be the equivalent of 0700 and is not done.

**Status.** `cargo check -p hir-ty --tests --target x86_64-pc-windows-msvc` passes on
Linux, so it type-checks against `windows-sys` 0.61 (a new dependency of `hir-ty` on
Windows only, already in the lock file through other crates). It has never been run. In
particular the layout of an access entry is declared by hand (`Entry`), and the set of
rights in `WRITES` is from the documentation.

**What the Windows session must run,** from the repository root in a normal (not
elevated) shell, then the first line again in an elevated one:

    cargo test --release -p hir-ty --lib -- store::tests
    cargo build -p blaze-engine --release
    mkdir C:\blaze-store-check
    set BLAZE_CACHE=C:\blaze-store-check
    target\release\blaze.exe cache stats
    icacls C:\blaze-store-check /inheritance:r /grant:r "%USERNAME%:(OI)(CI)F"
    target\release\blaze.exe cache stats
    mkdir %LOCALAPPDATA%\blaze-store-check
    set BLAZE_CACHE=%LOCALAPPDATA%\blaze-store-check
    target\release\blaze.exe cache stats

Expected: the tests pass, among them the three written for Windows
(`a_store_under_the_users_profile_is_taken`,
`a_store_whose_list_lets_others_write_is_refused`, which grants Everyone each write right
in turn with `icacls` and expects a refusal for each, and
`a_pack_whose_list_lets_others_write_is_not_read`). The first `cache stats` is refused
with "can be written to by other users" and the `icacls` line, since a directory at the
top of `C:` inherits write access for `Authenticated Users`. The second, after the
`icacls`, prints the statistics. The third, on a store under the profile, prints
them too. In the elevated shell the tests must still pass: there the files are owned by
Administrators. If the first `cache stats` is not refused, or the store under the profile is,
the check is wrong and I need the output of `icacls <the directory>`.

Not done on Windows: the socket (`socket.rs` is not mine to edit; finding 7 below stands),
and making the store private on creation.

## The daemon socket: findings to forward

Read at `origin/blaze-cargo` 13054e50d6 (`socket.rs`, `cargo/daemon.rs`, `cargo/client.rs`,
and the socket parts of `main.rs`). Nothing was edited. What is already right: the
directory is made 0700 and refused if it is not a directory, is another user's, or has any
group/other bit, using `symlink_metadata` so a link is not followed; the socket is 0600;
the peer's uid is checked (`SO_PEERCRED` on Linux, `getpeereid` elsewhere) before a byte
is read; the lock is taken before a stale socket is removed, so two daemons starting
together cannot unlink each other's socket; requests are capped at 16 MiB and 10 seconds;
each connection has its own thread so a slow one blocks nobody.

Read again this round at `blaze-collate` 1547078a30, where the file is now
`crates/blaze-engine/src/socket.rs`; still not edited. The list below was forwarded last round.
Of it, finding 1 still stands as written (`main.rs` calls `Served::bind(path)` on a
`--socket` path with no `private_dir`; the daemon's home and the worker directories do
call it, in `cargo/daemon.rs` and `cargo/client.rs`), and so do 2, 3 (no
`set_write_timeout` anywhere), 5 (`File::create(lock_of(socket))`) and 7 (the three
Windows functions are still `create_dir_all`, nothing and nothing).

Findings:

1. **Medium. A daemon started with `--socket PATH` does not check PATH's directory.**
   `private_dir` is only called for the daemon's home and the worker directories. The
   plain `blaze … --socket /tmp/x.sock` path (`main.rs`, `Served::bind(path)`) binds
   wherever it is told. The server side is still safe on Unix because of the peer check.
   The client side is not: `blaze sync SOCKET` connects to whatever is at that path and
   sends its request and, for workers, the whole environment of the client process
   (`read_request_and_env`). In a shared directory another user can be listening there
   first. Either call `private_dir` on the socket's parent in both the daemon and the
   client, or have the client check the server's uid the way the server checks the
   client's.
2. **Medium. Between `bind` and `chmod 0600` the socket has the umask's mode.** Inside a
   0700 directory this does not matter. With finding 1 it is a window in which another
   user can connect; the peer check then turns them away, so this is defence in depth
   only. Setting the umask around `bind`, or binding inside a private directory always,
   closes it.
3. **Low. No write timeout.** `read_lines_within` sets a read timeout and then clears it.
   Nothing sets a write timeout, so a client that sends a request and never reads the
   reply holds its thread, and whatever the reply path holds, until it goes away. Same
   user only. `set_write_timeout` next to the read one.
4. **Low. No cap on connections.** One thread per connection, no limit. A same-user
   process can exhaust threads. Same user only, so low; a bounded pool or a counter would
   do.
5. **Low. The lock file and the logs are created with the umask.** `File::create(lock_of(socket))`,
   `daemon.log` and the workers' `log` get 0644 on a usual system. They sit in 0700
   directories so nobody else can reach them, but the logs hold project paths, compiler
   diagnostics and whatever a build prints. Create them 0600 so that they stay private if
   the directory is ever copied or its mode loosened.
6. **Low. `private_dir` checks only the last component.** `DirBuilder::recursive` creates
   missing parents 0700, but an existing parent that another user can write to lets them
   swap the directory between the check and the bind. In practice the parent is `$HOME` or
   `$XDG_RUNTIME_DIR`. Worth a sentence in the doc comment, or a check that each parent up
   to the home directory is the user's or root's and not writable by others.
7. **Low. Windows.** `private_dir` is `create_dir_all`, `only_the_user_connects` and
   `check_peer` do nothing, on the argument that the profile directory's ACL is inherited.
   That holds for the default location and not for a `--socket` or home override outside
   the profile. Untested there. At least refuse a path outside the profile, or say so in
   the docs.
8. **Info. The status reply lists every worker's project root, checkout roots and log
   path.** Same user only, given the peer check. See the next section.
9. **Info. Worker ids are eight bytes of a SHA-256 of the checkout.** They name a
   directory under the daemon's home and select which running worker serves a request.
   They do not reach store keys. Two checkouts of one user colliding would share a worker
   directory; 2^32 checkouts is not a real number, but widening to 16 bytes costs nothing.

## What one client can learn about another checkout

Across users, after this round, I found nothing:

- The store is 0700/0600 and refused otherwise, so another user cannot read it, list it,
  time it or run `blaze-engine cache stats` on it. Before H2 they could do all four, and from the
  names in a pack learn the relative paths of the source files, item positions and the
  types functions were instantiated with.
- The socket turns away other users (on Unix), so status, diagnostics and build timing are
  not reachable.
- Logs live in the daemon's 0700 directory.

Across machines, through a remote, for anyone who can read it:

- Packs hold stable names: for checkout crates the content hash, file paths relative to
  the checkout (`app/src/main.rs`), item positions and the generic arguments of every
  instantiation; for crates cargo fetched, the path under `/cargo` (M6), which has the
  registry's index name and the crate's name and version in it and nothing of the
  pusher's machine. Machine code is of course there too.
- The checkout's own path is not there. Code whose data names the checkout (through
  `env!("CARGO_MANIFEST_DIR")` and the like) is kept out of shared entries by an existing
  check, and `storeguards.py` now measures that: 0 occurrences of the checkout path or the
  home directory in the store and in the pushed remote for the stress workspace, and 0 of
  either of two cargo homes for a workspace built on a registry crate.
- Whether a key is present tells a reader that someone built exactly that source. They
  have to have the source already to compute the key.

Between two checkouts of the same user, which is inside the trust boundary and was not
changed:

- `blaze-engine cache stats` gives counts, sizes and how long ago the store was used, not
  contents.
- A cache hit is faster than a miss, so one checkout can tell that another built the same
  function from the same source.
- The daemon's status lists other projects' paths, and diagnostics and logs have source
  paths in them.

## What is left

- **Freshness.** A machine with an empty store can be given any list ever signed, and a
  remote can keep a machine at the newest list it has seen. See the end of the rollback
  section; `BLAZE_REMOTE_FRESH` is a sketch, not code.
- **Windows was compiled, not run.** The store check needs the commands above run on a
  Windows machine. The socket there has no peer check, and the store is not made private
  on creation. The directory of turns is made 0700 on Unix and with what it inherits on
  Windows.
- **Real S3 was not run,** only MinIO on loopback. The bounds and the turns do not
  depend on the service, but that is an argument.
- **The two bounds of the lazy path** (keys held in memory, a pack over the store's
  limit) have no test of their own.
- **With `BLAZE_CACHE_MAX=off`** a pull writes as much as the lists name.
- **A writer to the remote can still delete a signed list with the bucket's own tools,**
  or anything else in it. Signing protects what is taken, not that there is something
  to take.
- **A pushed-over list loses lines.** When a signer finds a rolled-back or untrusted
  list it writes a new one with what it just sent, not with everything it ever sent;
  the next pushes add the rest back. That was the behaviour for untrusted lists already.
- **More than one cargo home in one build** (crates from two) is not remapped; such
  paths stay absolute and local.
- **Private registry names** are in the remapped path (`/cargo/registry/src/<index>/`).
- **`a_store_of_another_user_is_refused` needs root** to do anything, so CI as a normal
  user does not exercise it.
- **L1 and L4** are arguments. If blaze ever names an outside crate without its path, L1
  stops holding.
- **Same-user isolation** (one checkout's daemon against another's) is not a goal and
  nothing here provides it.
- **The socket findings** that still stand are for the session that owns `socket.rs`.

## Validation

All of this is on Linux x86-64, with stable 1.99.0, LLVM 20.1.8 and MinIO
RELEASE.2026-09-22T19-25-18Z, on the head of this branch after the merge of `blaze-collate`
at ce52276cf5. Nothing else was run.

Builds and checks:

    cargo fmt --all -- --check                                      clean
    cargo build -p blaze-engine --release                                  builds
    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 CARGO_TARGET_DIR=target-llvm \
      cargo build -p blaze-engine --release --features llvm                builds
    cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p blaze-engine -p project-model
                                                                    1022 passed, 0 failed, 1 ignored
    cargo test --release -p blaze-engine -- remote:: deps::                33 passed
    cargo check -p hir-ty --tests --target x86_64-pc-windows-msvc   checks

`cargo xtask tidy` fails, and not from this branch: it objects to two `#[should_panic]`
tests in `crates/blaze-engine/src/main.rs` that came with the build model merge and are on
`blaze-collate` as it is. `cargo clippy -p blaze-engine --all-targets` has no warning in
`remote.rs`, `remote/` or `deps.rs` that `blaze-collate` does not have (the prints, an
`Arc<Vec<u8>>` and two `chunks_exact`).

The harnesses, from `harness/`, one backend after the other, with `BLAZE_BIN` at the
release binary and `BLAZE_BACKEND=cranelift`, then `BLAZE_BACKEND=llvm BLAZE_OPT=2
BLAZE_EMIT=fast`:

| Harness | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 803 of 803 functions, 0 of 893 layouts differ | same |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 43 of 43, 5 open, 0 skipped | same |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 62 of 62 | 62 of 62 |
| `keys.py` | 46 of 46, 0 open | 46 of 46, 0 open |
| `debuginfo.py` | all right | all right |
| `deps.py` | 11 of 11 | 11 of 11 |
| `storekills.py --kills 100` | 100 kills, all verified | same |
| `storecorrupts.py --rounds 4` | 4 damaged stores rebuilt | same |
| `storefull.py` | 6 full disks, all built | same |
| `storeshared.py` | 180 builds | 180 builds |
| `storeguards.py` | all right | all right |
| `selects.py` | 3 of 3 | 3 of 3 |
| `remote.py` (MinIO; SeaweedFS is not on this machine) | 117 of 117 | 117 of 117 |
| `backends.py corpus runner cases --bin runner --levels 2f` | | exit 0 |
| `buildmodel.py` (runs both backends itself) | | 194 of 194, 0 WRONG |

The 5 open of `breakers.py` are the ones it lists as open on `blaze-collate`. Of the 117
checks of `remote.py`, 25 are new here.

Things in the gate that changed because of this branch, said here so they are not taken
for noise:

- `remote.py` failed one check after the merge ("the store is whole, and counts what
  was kept aside as a temporary file"): the stricter file names of M5 met the
  `<pack>.fetch.tmp` of the S3 remote. Fixed in `Kind::of`.
- `storeguards.py` failed after the merge on the mode of fetched packs (0644). Fixed in
  `transfer.rs`.
- `storeguards.py` failed after the round-three merge: it looks for paths in the bytes of
  the remote, and packs there are now compressed. It unpacks `.zst` objects before it
  looks.
- `deps.py` expected the old remote's words for a changed archive. It now expects
  "rejected units/<hash>" and one object counted as rejected.
- `storefull.py`'s largest disk is 768 KB since last round (the shorter standard-library
  paths made the store smaller than the 1 MB it had to overflow).
- Run at the same time in one checkout, the two backends' gates disturb each other
  ("no linker line"). The numbers above are from one after the other.
- The gate leaves about 7 GB under `/tmp` if nothing removes its scratch directories
  between harnesses; on this machine that filled the disk once, which showed up as
  `keys.py` failing to compile, not as a message about the disk.
