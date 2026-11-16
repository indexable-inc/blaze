---
status: plan
summary: The shared cache's read path (crates/blaze-remote/src/read, blaze-cli's store.rs). A command that misses a record locally reads it by byte range from an origin that lists and writes nothing. It checks each byte against a digest chain that starts at a signed head, writes the record through, and computes nothing the origin holds. Covers the sans-IO reader, the trust steps, what an origin must give, the first numbers, and what is not built.
date: 2026-11-16
---
# Reading records from an origin

## Objects

An origin holds five kinds of object. Every name is built from a digest's hex only
(`Object::name`), so nothing fetched ever becomes a path.

| object | what | checked by |
|---|---|---|
| `head` | 160 bytes: namespace, generation, the manifest's digest and length, the signer, an Ed25519 signature over the first 96 bytes | the signature, against the keys given (`HeadCheck`) |
| `manifest-<d>` | one 104-byte row a segment: its digest, length, index geometry, and its summary's digest and length | BLAKE3 against the head |
| `sum-<s>` | a segment's Fuse8 filter (about 9 bits a key), then one fence a page: the page's first key and its digest | BLAKE3 against the row |
| `idx-<s>` | the segment's entries, each the store's 48-byte entry plus a 32-byte value digest, in pages | each page's BLAKE3 against the summary |
| `seg-<s>` | the plain segment itself, unchanged; payloads are read from it in place | each payload's value digest from its index entry |

The index adds a value digest because the plain store's entry holds only a CRC-32C. A CRC
proves nothing about bytes read by range from someone else's file.

`publish` (pure) seals segments into these objects. The head is written last, so an origin is
filled by adding objects and then replacing the head. `examples/seal_origin.rs` is the stand-in
for CI's writer.

## The machine

`Reader` is sans-IO. `begin(keys)` starts a batch. `poll(now)` hands out asks: object, offset,
length, deadline. `replied(id, n)` takes each answer back. The phases run in order:

1. The head, then the manifest. Both are read once per command.
2. The summary of each segment the batch may touch. Each key goes through the filter, then
   the fences, so it costs one index page or none.
3. The index pages, deduplicated and capped.
4. The payloads, sorted and coalesced into ranges. A gap may be at most `gap_most` and a range
   at most `range_most`.

`Limits` bound the work:
- keys a batch;
- the size of a record, a range, and a batch's bytes;
- asks a batch and asks in flight;
- pages;
- a deadline per ask and per batch.

The head and the manifest are refused by their own bounds (`MANIFEST_MOST`, `ROWS_MOST`,
`SUMMARY_MOST`, `PAGE_ENTRIES_MOST`).

`fetch` drives a reader against any `Origin`. That trait is one call: read a byte range of a
named object, by a deadline. A folder (`FolderOrigin`, bstd files) and the simulation's origin
are built.

## Not fetched

A key whose read fails gets a typed `Unfetched`:
- `Origin`: the origin answered missing or unreadable;
- `Short`: fewer bytes came back than were asked;
- `Late`: the reply came after its deadline;
- `Refused`: a digest, signature, namespace, format or bound did not match;
- `Limit`: a size or count was past its bound.

A segment that fails is not read again in this command. `Layered` prints one stderr line per
kind, naming the record and the reason, and the drive computes the record. This is not a
fallback (rule 3). The local computation is the record's definition. The origin is a cache of
it, byte for byte (rule 25), so both routes end in equal bytes. What never happens is a second
try, another origin, or a refused byte used anyway.

## Trust

Each step below has a hostile test in `read/tests.rs`:

- **Sizes before allocating.** Every length from the wire is checked against its bound before
  a buffer is sized from it. Tests: a signed head naming a huge manifest; a manifest claiming
  four billion rows; a summary whose lengths disagree. Planted bug `SizeFromWire` is caught.
- **Digest before use.** The manifest, summaries and pages are each checked against their
  parent's digest, and every payload against its value digest, before it is answered or
  written. Planted bug `PayloadBeforeDigest` is caught.
- **Signed head.** A head is believed only when it is signed by a given key through `HeadCheck`.
  Tests: an unknown signer, every byte flipped one at a time, another namespace, and a forged
  origin in the simulation. Planted bug `HeadUnsigned` is caught.
- **No fetched bytes in paths.** Names are only `[a-z0-9-]`, which is property-tested.
- **Decoding with limits.** Rows must be sorted, and each entry's payload must lie inside its
  segment's payloads. Test: an entry placing its payload outside the payloads.
- **Ranges.** Planted bug `RangeOffByOne` is caught.

Every signature blaze-remote checks (the head through `HeadCheck`, statements, roots, key
lists, bundles) is verified by `crates/blaze-ed25519` (next section), in every shape of the
crate. A refusal names the object and carries blaze-ed25519's `Refused` (`SignatureWhy::Invalid`,
`Why::Signature`) and is never retried. The one place the reason is dropped is a statement's:
blaze-keyed's `Refused::Signature { signer }` has no field for it yet. `tests/signed.rs` checks
this in the shape the binary links (no `sign`) on checked-in signed bytes: the genuine key list,
statement and head are taken, and a flipped byte of message, signature or key, another domain
and a signer outside the key list are each refused with the typed reason.

### Ed25519, written here

`crates/blaze-ed25519` is RFC 8032 verification and nothing else: SHA-512, the field mod
2^255 - 19, edwards25519 and Straus's double-scalar multiplication, `no_std`, no allocation,
no unsafe code, no third-party crate. Verification touches only public values, so
variable-time code is sound; that is why it is ours.

What is checked, in order (each refusal is a `Refused` variant naming the step):
1. the key decodes canonically: y below p, not x = 0 with the sign bit set, on the curve;
2. the key is not of small order;
3. the key has order exactly L (no honest key has any other);
4. S is below L (else S + L is a second signature);
5. R decodes canonically;
6. R is not of small order;
7. k = SHA-512(R || A || M) mod L, and [S]B = R + [k]A, compared as points.

Steps 1 to 3 run once per key (`VerifyingKey::decode`), 4 to 7 per signature.

Edge-case decisions: the cofactorless equation, as RFC 8032 implementations commonly use, so
every signature a standard signer makes is accepted and one that only a cofactored (ZIP-215)
verifier takes is refused. Small-order keys and R are refused, as libsodium does. Mixed-order
keys are refused too.

What is tested, each against a verdict and a reason:
- RFC 8032 section 7.1's five tests (TEST 1, 2, 3, 1024 and SHA(abc)), and each one's secret
  key re-derived and re-signed by `tests/data/edge.py`;
- the 12 cases of "Taming the many EdDSAs" (Chalkias, Garillot, Nikolaenko, 2020), from the
  paper's own `cases.json`, and Wycheproof's `ed25519_test.json` (117 valid, 64 invalid, 29
  acceptable). Both are vendored as published in `tests/data` with their URL, revision and
  license (its README); `convert.py`, run by hand, checks each label against the reference in
  `edge.py` and writes `published.txt`, which `tests/published.rs` reads (222 lines);
- edge cases built by `edge.py`: the eight small-order points in every encoding, the seven
  keys of mixed order (a prime-order key plus each torsion point), non-canonical y, S at and
  around L, R as a negative zero;
- planted bugs: each tempting wrong check (identity by x alone, torsion by [4L], equality by x
  alone, S = L taken, S checked on its top limb only, an unnormalised comparison, no range check
  of S, non-canonical R decoded) is a control the oracle must catch, and an independent review's
  14 mutants of the source all die.

The public surface is `verify`, `verify_parts`, `VerifyingKey` (`decode`, `as_bytes`,
`verify`, `verify_parts`), `Refused`, `Encoding` and `sha512`. The field, the scalars and the
points are private to the crate; `cost` is a doc-hidden door for `examples/cost.rs` alone.

What `HeadCheck` calls: it signs `domain || 0 || body`, so it passes the three pieces and copies
nothing:

    blaze_ed25519::verify_parts(&signer.0, &[domain.as_bytes(), &[0], message], signature)
    // -> Result<(), blaze_ed25519::Refused>; `verify(public_key, message, signature)` for one piece

An `Err` is `SignatureWhy::Invalid(Refused)`, so the error names the failed step (rule 19).
`Ed25519Keys` may later decode its keys once with `VerifyingKey::decode` and call
`key.verify_parts`, which saves 14 us a head.

Cost (`examples/cost.rs`, 2026-11-16, the owner's Mac shared with other lanes, best of seven):
16.4 us a verification with the key decoded once, 30.3 us with it decoded each time. The floor
is the counted field operations at measured cost, with the square root's chain at latency;
the ratio is 1.13. SHA-512 runs at 772 MB/s, scalar.

Deliberately absent: signing, key generation, batch verification, and anything that holds a
secret.

**Signing stays on `ring` (decision, 2026-11-16).** blaze writes no signer: code that holds a
secret key must be constant-time and resist side channels, which verification need not, and a
signer of our own would add that risk for no speed the edit loop sees. So `Signer` (a builder's
or an anchor's seed), the encoders of signed objects, pushing and `read::publish` use `ring`
0.17, and blaze-remote links it only under its `sign` feature (`bucket` turns it on, as SigV4's
SHA-256 and HMAC are `ring`'s until they move to `crates/blaze-sha2`). The `blaze` binary takes
blaze-remote without default features, so it verifies with blaze-ed25519 and links no `ring`.

## Proof

The simulation draws random stores (1 to 4 segments, pages of 1 to 8 entries, records that
appear twice), batches, and origin misbehaviour. A misbehaving reply is slow past its deadline,
absent, unreadable, short, flipped, or never answered, and replies are reordered. Some cases
also use a head that names a segment whose object is gone, a head forged by another key, or
small limits.

The oracle checks that every answer is:
- the exact bytes, or absent only when the key is not held, or a typed miss;
- complete when the origin is calm and whole.

It also checks that no ask passes its limit and that no batch outlives its deadline.

A crash test cuts the write-through at any put. The local store then holds only true bytes, and
a restart finds the rest.

## Measured (2026-11-16, release blaze from greenmix ce3ca627c8, Mac shared with other lanes)

The test package is 14 files of library/core/src, 13,177 lines. A cold `blaze check` makes 16
runs and 19 records (288,400 bytes). Those were sealed into one segment (5 objects).

**A copy with an empty `.blaze`, run with `--origin`:**
- The run line: `0 runs, 1 store reads (1 found), certificate stood`.
- The origin line: 5 range reads (head, manifest, summary, index, payload), 2,084 bytes for 76
  used, 0.43 to 0.80 ms.
- Time in main: 3.2 to 3.7 ms, against 11.1 to 11.8 ms computing locally.

**The same copy with one file edited:**
- The run line: `1 runs, 17 store reads (15 found)`.
- The origin line: 19 range reads (1 head, 1 manifest, 1 summary, 1 index, 15 payloads),
  281,128 bytes for 279,120 used, which is 1.007 of the bytes used.
- All 15 fetched records equal the first checkout's bytes.
- Over 7 pairs, the median time in main was 22.0 ms with the origin against 29.0 ms locally.
  The spread was 9 to 41 ms against 16 to 35 ms, because the machine was loaded.

**The floor:**
- For a batch: one head, one manifest, one summary and one index page a segment, then the
  payload ranges.
- For gets of one key at a time, as blaze-drive asks today: 4 + 15 = 19 reads. That is a ratio
  of 1.00.
- If the 15 keys came as one batch, the floor is 5 reads (the payloads coalesce into one range;
  `a_batch_of_adjacent_payloads_is_few_ranges` shows 300 keys in 1 range). Measured against
  that, the ratio is 3.8. Closing it needs the drive to emit its Gets together.

## One chain of stores (spec, 2026-11-16)

A get that misses the project's store (`<checkout>/.blaze/v2/packs`) goes down one chain, each
link the same pack store or an `Origin`, and stops at the first that holds the key:
1. **The machine store**, `<home>/.blaze/v2/shared/`: a pack store like the project's (one pack a
   command, named by its digest, published unsynced, read whole once per boot). It holds only
   records this machine computed whose key holds nothing of a project: by target.md
   "Namespaces", a record is public when its name and every name it reads are a toolchain
   crate's (a registry archive known by its checksum is next). Today that is the `CrateScope`
   records of core, alloc, std, bstd and their dependencies. A put of a public record goes
   here, every other put to the project's store. Two builds racing write equal bytes under one
   key; either pack stands (the store's rule). Not `v2/records`: an old one-file-a-record
   layout used that name, and the open removes what it cannot read.
2. **The named origins**, a folder or an HTTP base address, checked as above. A fetched record
   is written through to the project's store, never to the machine store, which holds only
   what this machine computed.

The home is a value the command's start passes in (rule 75); the machine store is opened
through the effect layer as a second tree over it (`Fs::home`), wherever it is; a home that
cannot be opened turns the machine store off with one line naming it.

The floor for a second project's cold build: no toolchain task runs; each machine pack costs
one listing, one footer and index read (plus one whole read once per boot), and each record
one `pread`; the rest is the project's own crate.

## Shared across projects (2026-11-16)

**Built** (blaze-cli store.rs; names.rs `tell_public`; one call in work.rs and main.rs): the
chain above. `PackStore::open_in` opens either store; `Sharing` is the seam (`ToolchainCrates`
for the command); `Machine::of` places the store from `HOME` and `PWD`; the names step tells
the store the world's toolchain crates wherever a world is made.

**Proof** (`mixcheck test blaze-cli store::tests`, "13 passed, 0 failed"): the chain's
simulation cuts power and fails effects anywhere, the oracle checks every get through the chain
and that the machine store holds no project record:
`test store::tests::a_crash_anywhere_in_the_chain_keeps_records_whole_and_the_machine_store_public ... ok`;
planted bugs caught: `the_chain_oracle_catches_a_project_record_in_the_machine_store ... ok`
(`PrivateIntoMachine`) and `the_chain_oracle_catches_a_machine_pack_taken_unverified ... ok`
(`VerifySkipped` in the machine store: its packs are unsynced, so the once-a-boot read is what
keeps torn bytes out).

**Measured** (release `blaze-shared` from greenmix 5018ef3617, fresh home with only the
toolchain shelf, two hello-world copies differing in package name, load 11.1):
- A cold: `37 runs, 20 store reads (0 found)`, `2769.365 ms in main`, 573,981,848 bytes peak
  footprint, `machine store: ... 27 written`, "names resolved for 15 crates".
- B cold: `9 runs, 33 store reads (27 found)`, `30.909 ms in main`, 106,644,200 bytes peak
  footprint, `machine store: 37 records found, 0 written; 1 packs, 1 read whole`; no names
  line (no crate resolved here). Before (same binary, no machine store, experiment e1): B paid
  `37 runs`, 3174.476 ms, 682,919,136 bytes. 90 times faster, 6.4 times less memory.
- Floor: B's 9 runs are its own (units, graph, its files); the toolchain costs one pack read
  whole (Boot is `Unknown` until bstd has a boot fact, so every command reads the machine pack
  whole: that is the gap to the floor, not measured apart yet).

**Built, second runner (2026-11-16):**
1. *The home wherever it is.* bstd `Fs::home(cx, home, folder) -> Result<Dir<ReadWrite>,
   HomeError>`: natively a second driver over the absolute `home` (`Io::beside`: one file
   thread, no environment, nothing above it); in the simulation a folder beneath its one root.
   `HomeError { home, why: NotAbsolute | Dotted | Open | Fs | NoSecondTree }` names the home.
   `Machine::of(cx, fs, home)` opens `<home>/.blaze` through it (the machine store is
   `v2/shared` in it); a refusal is `Machine::Off(MachineOff::Home(..))` and the one stderr
   line. Tests: bstd `tests::home` "2 passed" (a native home outside the start folder in a
   temp folder; the simulation's); blaze-cli `store::tests` "14 passed, 0 failed", the chain's
   crash simulation now with the project at `work/project` and the home at `/home/sim`, and
   `a_home_that_cannot_be_made_turns_the_machine_store_off_by_name`.
2. *Once per boot.* The boot record (`boot` beside the packs, naming the boot and the packs
   known whole in it) and its simulations already existed (`a_pack_is_read_whole_once_a_boot`,
   the planted `VerifySkipped` and `SelfVouching`, a reboot drawing a new boot at each power
   cut); what was missing was the fact: main.rs passed `Boot::Unknown`. It now passes
   `Io::boot()` (macOS `kern.bootsessionuuid`), and a `NoBoot` says one line and verifies.
3. *Registry crates public.* `Names::public` holds the toolchain's crates and every crate whose
   lock package is a registry source with a `checksum`; the store's rule is unchanged (a record
   is public only when its name and every name it reads are public), so a registry crate that
   reads a member, a path or git package (a `[patch]`) stays the project's. Not yet in the
   chain oracle: the simulation's `EvenLengths` stands for the rule, not the names.

**Next, in order:**
1. HTTP origin: an `Origin` over blaze-h1 ranged GETs through bstd `Net` (`Net::connect`,
   `Conn::recv`; blaze-h1 `write_request` with `Range`, `Response::new_in(Limits, ..)` bounds the
   head and body before reading, `Response::ranged(asked)` checks `Content-Range`; a 3xx is
   refused). blaze-fetch's own path is blaze-http's driver thread, not bstd effects: do not
   reuse it. Test: a lying 127.0.0.1 server (wrong payload bytes) caught by the digest chain.
2. Batched gets (the driver's, main.rs's drain at `Effect::Get`): take every `Effect::Get` the
   machine has queued in `effects` at once, then
   `Store::get_many(&mut self, cx: &Cx, keys: &[Key], into: &Frame, got: &mut dyn FnMut(Key,
   Option<&[u8]>) -> Result<(), CliError>) -> Result<(), CliError>`: local then machine store
   per key, the misses as ONE `fetch(reader, origin, clock, &missed, alloc)`, then `got` for
   each key in the order asked, which steps `Event::Got`. Floor 5 origin reads where
   one-at-a-time gets make 19.

## Not built

- **Cache writes.** Only CI writes an origin, through `publish`. Nothing here uploads.
- **An HTTP origin through blaze-h1.** `Origin` is the seam. Its replies arrive out of order, so
  `poll` and `replied` already work without `fetch`'s one-at-a-time loop.
- **Peers.**
- **Chunking large records.** A record past `record_most` (64 MiB) is a typed miss.
- **Compressed payloads.** columns.md, "Cold and moving bytes, measured", found two good options:
  - solid zstd blocks of about 64 KiB, which cut small records 2.3 to 3.4 times;
  - per-kind dictionaries, which cut them about 2.8 times while keeping one read per record.

  With either, the index entry names the record's block (or dictionary) and its place inside
  the decompressed bytes. The payload ask becomes a block ask with a bound on the decompressed
  size, and the value digest is checked after decompressing, as now.
- **Batched gets from the drive.**
- **Keeping the origin's open files across asks.** Each ask opens its object again.
- **The reader's tables live in the command's frame,** so the bytes fetched stay until the
  command ends, as the drive's record buffers already do.
