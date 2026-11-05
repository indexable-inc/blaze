---
status: record
summary: The store on disk: what was hardened and measured.
date: 2026-10-20
---
# The store on disk: what was hardened and what was measured

Branch `blaze-disk-cache`, from `blaze-collate` at `b19fdfacd8`. Linux x86-64, 8 cores,
Rust 1.99.0, LLVM 20.

The store is the directory `BLAZE_CACHE` names. It holds compiled functions (and the
entries that say which function a request resolves to) by a hash of what they were
compiled from. The code is in `crates/hir-ty/src/mir/codegen/store.rs`; the `blaze-engine cache`
commands are in `crates/blaze-engine/src/remote.rs`.

## What is on disk now

    version                 "blaze store 2"
    <sha256>.pack           a sealed pack: 8 bytes of magic, then records
    <pid>-<nanos>.open      the pack one daemon is appending to
    *.tmp                   a file on its way to another name
    lock                    held while the directory is emptied or collected

A record is a 32-byte key, the length of the value, a CRC-32 of key, length and value,
and the value. A sealed pack is named by the SHA-256 of its bytes and is never written
again.

## 1. Format version and identity

`version` names the format of the directory, and the last byte of the pack magic repeats
it. A daemon or a `blaze-engine cache` command that opens the directory does one of three things:

- same format: uses it;
- older format, a `version` it cannot read, or packs with no `version` (the format before
  this work had none): takes `lock`, removes the files that are the store's by name (packs,
  open packs, temporaries, `version`), writes the new `version`, and starts empty. Files
  that are not the store's are left;
- newer format: touches nothing, says `store: not used: the store is of format N, and this
  blaze reads format 2`, and compiles without a store. `blaze-engine cache gc|push|pull` fail with
  the same reason.

There is no migration: the contents are a cache, so starting empty loses time and nothing
else. A pack of another format that turns up under the current `version` (an older daemon
still running and sealing into the directory, or a pull from a remote) fails the magic
check and is removed, or in the case of a pull is not written.

Keys on disk are not the caller's keys. Each is hashed with an identity, which is the hash
of the format number, the SHA-256 of the running `blaze` executable, and the toolchain:
the `libstd-*` file names of the sysroot the program links against (their hashes differ
per toolchain build), rustc's commit, and the `rustc_driver-*` library name. A daemon of
another build, or one compiling against another toolchain, computes other keys and finds
nothing; it can share the directory without ever reading the other's values. If the
toolchain cannot be told, the daemon says so and keeps no store. The executable hash used
to be mixed into the entry keys alone, in `cached.rs`; it now covers everything, in one
place.

A consequence, not new: packs are only useful to a machine with a byte-identical `blaze`
and the same toolchain, which is also true of `blaze-engine cache push|pull`. Records of builds
that no longer exist stay until they are evicted as the least recently used.

## 2. Crash safety

Every file that appears under a name others read gets there by rename, after its bytes
are written and synced, and the directory is synced after the rename:

- a pack sealed by its daemon: the open pack is synced, then renamed to its hash;
- a merged pack, a pulled pack, `version`, and a pack or manifest pushed to a directory
  remote: written to `NAME.<pid>-<nanos>.tmp`, synced, renamed (`write_whole`). The old
  `write_whole` in `remote.rs` had no sync and a fixed temporary name per process; it is
  gone.

An open pack is appended to without a sync per record. After a kill its tail may be a
part of a record. Whoever next opens the directory finds that nobody holds the lock on
the file, cuts it at the last record whose checksum holds, and seals that. Temporaries
nobody holds are removed once they are a minute old.

`harness/storekills.py` kills daemons with SIGKILL: while they start on an empty store
(cold build), while they start on the store the last kill left, during an edit, around
the moment the answer to an edit is sent, and at the first, second or third moment the
store directory is seen to change (an open pack growing, a pack getting its name, a
merged pack on its way). Daemons are started with sizes that make them seal, merge and
evict on nearly every build. After each kill `blaze-engine cache verify` must find nothing that
is not what was written, a new daemon on the same store must produce a program that
prints what cargo's build prints, and verify must pass again.

## 3. Integrity

The hash of a sealed pack is checked against its name the first time a process reads it,
together with the magic and the framing of every record (a pack with bytes after its
last whole record fails). The checksum of a record is checked every time its value is
read, so damage to a pack after a daemon has indexed it is caught as well. A pack that
fails either check is dropped from the index and unlinked, the daemon prints
`store: NAME ...; removed`, counts it, and compiles what was in it. `blaze-engine cache push`
does not send a pack that is not what its name says.

`harness/storecorrupts.py` damages one file per round: a bit flipped, the file cut
short, bytes added, a stretch zeroed, or the whole file replaced; with no daemon
running, under a daemon that has already read the pack, or on an open pack. It then
checks that verify reports the damage, that a daemon builds the right program from the
damaged store and says it removed the pack, and that verify is clean afterwards. It
ends with stores of an older format, of no format, with unreadable `version` files, and
of a newer format.

## 4. Size limit and garbage collection

`BLAZE_CACHE_MAX` is the most the sealed packs may take: a number of bytes or one with
`K`, `M` or `G`, `off` for no limit, 32G when unset. `BLAZE_CACHE_SEAL_AT` (4M) is the
size at which an open pack is sealed at the end of a build; it exists for the harnesses.

A collection runs when a daemon opens the store and after it seals a pack, and on
`blaze-engine cache gc [--max SIZE]`. It seals open packs left by daemons that are gone, removes
old temporaries, merges sealed packs under 4M into one when there are more than 16 of
them (the new pack is in place before the old ones go), and then unlinks the least
recently used packs until the rest fit. "Used" is the modification time of the pack,
which a daemon sets at the end of a build in which it read a value from that pack. The
unit of eviction is the pack, not the record.

Eviction is safe for readers because a daemon keeps the packs it has indexed open: an
unlinked pack stays readable to it until it exits. A daemon that starts later does not
see the pack and compiles what was in it.

    blaze-engine cache stats      format, packs, bytes, open packs, temporaries, limit, oldest and newest use
    blaze-engine cache gc         collect now, to BLAZE_CACHE_MAX or --max SIZE
    blaze-engine cache verify     read everything; exit 1 if any pack is not what was written

Full disk: any error while adding to the store (ENOSPC among them) stops the writer for
that build. The daemon prints `store: nothing more is kept of this build: No space left
on device ...` once, counts the values it did not keep and prints the count at
the end of the build, and goes on compiling; the next build tries again. If the
directory cannot even be claimed the daemon prints `store: not used: ...` and runs
without one. `blaze-engine cache pull` into a full store fails with the error and leaves no
partial file.

`harness/storefull.py` mounts an 8 MB tmpfs (it needs `sudo -n mount`), fills all but
0, 4K, 16K, 128K, 512K and 1M of it, and for each builds and edits under a daemon whose
store is there. It then makes room and checks a new daemon keeps and finds its code
again, that pull into the full store fails cleanly, and that `gc` works with no room.

## 5. Concurrency

Lock-free by construction for everything a build does, with one lock for housekeeping.

- Reading takes no lock. A sealed pack appears whole by rename and never changes.
- Adding takes no lock on the directory. Each daemon appends to an open pack of its own
  name and holds an advisory lock on that file for as long as it runs, which is how
  others tell a live open pack from an abandoned one. Two daemons that compile the same
  function each write it; the merge keeps one.
- Removing is unlink, which leaves the file whole for those that have it open.
- `lock` is taken (non-blocking for collection, blocking for a change of format) so that
  two collections, or a collection and an emptying, do not run at once. A collection
  that finds it held does nothing; `blaze-engine cache gc` then exits with "another process is
  collecting the store".

Locks are `File::lock`/`try_lock` from std (flock on Linux). On a network file system
where those do not work across machines, two collectors could run at once; the worst
that follows is a pack evicted twice or merged twice, not a pack with wrong bytes, since
names are hashes and writes are whole. That is reasoning, not something measured.

`harness/storeshared.py` runs 3 daemons with a checkout each on one store, building the
same states at the same time and being killed and restarted every third build; first
with room, then with a 1500K limit while `blaze-engine cache gc` and `blaze-engine cache verify` run in
loops beside them.

## What was measured

Binaries: `cargo build -p blaze-engine --release` and
`LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p blaze-engine --release --features llvm`.
All commands from `harness/` with `BLAZE_BIN` set; "Cranelift" is `BLAZE_BACKEND=cranelift`,
"LLVM" is `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`.

Kills (`python3 storekills.py --kills N`, 4 workers):

| Backend | Kills | cold / start / edit / settle / write | Killed as the store changed | Stores left with an open pack | with a torn record | with a temporary | Failures | Time |
|---|---|---|---|---|---|---|---|---|
| Cranelift | 400 | 45 / 117 / 108 / 44 / 86 | 86 | 123 | 1 | 0 | 0 | 583 s |
| LLVM | 200 | 36 / 38 / 50 / 17 / 59 | 59 | 94 | 0 | 1 | 0 | 351 s |
| Cranelift | 100 | 16 / 33 / 24 / 13 / 14 | 14 | 27 | 0 | 0 | 0 | 164 s |
| Cranelift | 100 | 13 / 17 / 25 / 10 / 35 | 35 | 42 | 0 | 0 | 0 | 171 s |

That is 800 kills with the committed code, 0 failures. Earlier runs during development
(592 kills on a binary that differs only by formatting, without or before the final
`write` kind) also had 0 failures.

Before the `write` kind of kill existed, a 400-kill run also passed, but left no torn
record and no temporary in any store: random moments almost never fall inside a write.
That is why the kind was added. Even with it, a torn record was left once and a
temporary once in 800 kills; the recovery from both is otherwise covered by the unit
tests `a_torn_open_pack_keeps_its_whole_records` and
`what_is_left_on_the_way_to_a_name_is_removed_once_it_is_old`.

Damage (`python3 storecorrupts.py --rounds N`): Cranelift 10 rounds (5 with no daemon,
2 under a daemon, 3 on an open pack) plus 4 older or unreadable formats discarded and 1
newer refused, 226 s, pass. LLVM 4 rounds plus the same formats, 161 s, pass.
Two more Cranelift runs of 4 rounds each passed (152 s, 177 s).

Full disk (`python3 storefull.py`): 6 amounts of room, 12 builds, all right; with 0
bytes the store was not used at all, with the others the daemon kept what fit (0 packs
at 4K and 16K, 1 at 128K, 7 at 512K, 14 at 1M) and said what it dropped. Cranelift 38 s,
LLVM 39 s, pass. Two more Cranelift runs passed (35 s, 34 s).

Shared (`python3 storeshared.py --rounds 12`): 72 builds per run, all right. Cranelift:
360 collections evicted 117 packs while 8734 verifications found nothing wrong. LLVM:
284 collections, 109 packs evicted, 1863 verifications. In both, a daemon that had
compiled nothing found its code in what the others sealed and compiled 0 functions.
Two more Cranelift runs passed: 310 collections, 75 evictions, 8306 verifications; 286
collections, 66 evictions, 8074 verifications.

Unit tests: 19 in `store.rs` (format, identity, a bit flipped at six places in a
pack, a pack cut at five lengths, damage after indexing, LRU order, the limit, merging,
shared directories, whole writes, ENOSPC on `/dev/full`).
`cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p blaze-engine`: 928 passed,
0 failed, 1 ignored. `cargo fmt --all -- --check`: clean. Clippy on `hir-ty` and
`blaze` reports nothing in the changed files beyond the `eprintln!`/`println!` lints the
daemon's code already has.

Regression gate, both backends, all exit 0:

| | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 803/803, 0 of 892 layouts differ | MATCH, 803/803, 0 of 892 |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99 | 99 of 99 |
| `breakers.py` | 25 of 25, 0 skipped | 25 of 25, 0 skipped |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 52 of 52 (58 of 58 after the merge) | 52 of 52 (58 of 58) |
| `keys.py` (after the merge) | 26 of 26, 4 open | 26 of 26, 4 open |

Remote, by hand: a daemon with `BLAZE_REMOTE` set to a directory pushed 1 pack (1769
records); a second daemon with an empty store pulled it and compiled 0 functions.

The four store harnesses are added to `.github/workflows/blaze.yaml` (100 kills, 4
rounds of damage). On the first push of this branch (run 39025671387) all four passed
on GitHub's runners under both backends, and so did fmt and the tests. The run as a
whole failed on `difftest.py` and one case of `breakers.py`, because the workflow set
`RUST_BACKTRACE` and the backtraces of rustc's program and blaze's differ; that is
fixed on `blaze-collate` (`e64b0d2d8f`), which is merged into this branch.

The merge also brought `keys.py`, which builds with the store another build filled
after each thing code depends on has changed. With this branch's store: 26 of 26
right and the 4 cases it lists as open, under both backends.

The run on GitHub after the merge (39028503621) passed whole: fmt and tests, and every
harness including the four for the store, under both backends.

## What went wrong on the way

- The toolchain identity first came from VFS source paths alone and was empty for these
  workspaces, so the daemon refused to use the store. It now uses the sysroot's symbol
  files, rustc's commit and the driver library name.
- The damage harness first demanded that a damaged pack be absent afterwards. It is
  removed, but the same code compiled again seals to the same name, so the file is back;
  the harness now checks the daemon's own statement of removal.
- The shared harness first restarted only one daemon. A daemon writes most of the store
  when it builds everything at start, so the others added little; all are now restarted.

## Still open

- Linux x86-64 and Windows x86-64 only; the harnesses were not run on macOS. On Windows
  a pack that another daemon has open is evicted like any other: std opens files shared
  for delete and removes them the POSIX way, so the name goes at once and the daemon
  that has it open reads on (NTFS; a file system without that keeps the name until the
  last handle closes). See WINDOWS.md for what the Windows run found.
- No power-loss testing. SIGKILL does not lose page cache, so the syncs before renames
  are by construction, not by measurement.
- Advisory locks over NFS, as said above.
- Eviction is by pack. One recently used record keeps its whole pack (up to 64M); a
  limit smaller than a few packs evicts nearly everything.
- Records of builds of blaze that no longer exist cannot be told from live ones and
  leave only by LRU.
- A newer-format store is refused, not shared: an old and a new blaze pointed at one
  directory means the old one runs without a store.
- The S3 remote path was not exercised (no bucket); only the directory remote was.
- `storefull.py` needs passwordless sudo to mount a tmpfs.
