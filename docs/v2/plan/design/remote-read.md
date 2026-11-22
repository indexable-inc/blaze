---
status: plan
summary: The shared cache's read path (crates/blaze-remote/src/read, blaze-cli's store.rs). A command that misses a record locally reads it by byte range from an origin that lists and writes nothing. It checks each byte against a digest chain that starts at a signed head, writes the record through, and computes nothing the origin holds. Covers the sans-IO reader, the trust steps, what an origin must give, the first numbers, and what is not built.
date: 2026-11-16
---
# Reading records from an origin

Scope since 2026-11-17 (decisions D130, then D132 the same day): the origin serves the records blaze itself
publishes (the toolchain's crates: std, core, alloc, per platform and toolchain, written by CI). A project's own
records are not shared between machines. Built after `check` runs on the new engine.

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
   here, every other put to the project's store (superseded: "Shared across worktrees"). Two builds racing write equal bytes under one
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

The machine store's counts are profile rows now (`machine found .. written ..; packs .. read
whole ..` in `.blaze/v2/profile.txt`), not a stderr line: `a_clean_run_says_nothing_of_its_own`
"1 passed, 0 failed".

**Measured** (release `blaze-shared`, greenmix 2447ae6fec, `exp2.sh`: home a sibling of the
projects, never beneath the start; load 52 to 55, so times are not comparable with e2's):
- e3 (hello): A cold `37 runs, 20 store reads (0 found)`, 10604.145 ms; B cold `9 runs, 33 store
  reads (27 found)`, 48.974 ms, 104,481,536 bytes; no "machine store off" line; the home holds
  `v2/shared/becd65ae35c954f6` and `boot`. B again: `0 runs`, 1.280 ms, profile row
  `machine found 0 written 0; packs 1 read whole 0`.
- e4 (hello + cfg-if 1.0.4 from the registry): A cold "names resolved for 16 crates", `42 runs`,
  4575.374 ms, 701,761,072 bytes; B cold `11 runs, 36 store reads (29 found)`, 238.628 ms,
  109,593,704 bytes, and NO names line: cfg-if's records came from the machine store too.

**Next, in order:**
0. MEASURE what is left (the whole-pack read's share: B cold in a NEW boot against the same
   boot; and at low load). Earlier note, kept: (not done: the first greenmix waited 1200 s on the build lock; a second was started
   at hand-on and keeps its binary as `blaze-shared`): `port27/shared-1116/exp2.sh e3
   <blaze-shared> proto` and `exp2.sh e4 <blaze-shared> proto-cfg` (a home beside, not under,
   the projects; `check .` from inside each; b run twice). Quote: no "machine store off" line;
   b's second run's profile row `read whole 0` and its gate time against e2's 30.670 ms;
   proto-cfg b cold "names resolved for 1 crates". Then tell the driver lane (OWNERS.md) the
   `UnitGraph` row (22.896 ms) in one message.
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

## Shared across worktrees (2026-11-16)

The owner: not only the standard library, every library, and a project's own code, shared
between worktrees. **What the keys hold** (searched: blaze-drive key.rs, every `Name::file` and
`cert_key` caller, blaze-world `unit_input`, `unit_graph_task::source`, names.rs `world`,
blaze-cli for `PWD`, `mtime`, `stamp`, `absolute`): no key and no record holds the checkout's
path, an absolute path or a stamp. A file's name is its checkout-relative path (key.rs:170); a
unit's key holds its root relative to its package (blaze-unit-graph lib.rs:169) and the lock's
`source` text, empty for a member; the certificate key is the command and each file's relative
path and digest (key.rs:275); `Tool` is the binary's digest. Stamps live in the leaf table,
`.blaze/v2/last` (sweep.rs:556), outside the store; `PWD` is printed only (print.rs:27). So two
worktrees with the same texts compute the same keys, and sharing is only where a record is put.

**Design.** A get goes down the chain: the checkout's store, the public machine store
(`v2/shared`), the user's own store (`v2/own`), an origin. A put goes where `Sharing::place`
says (store.rs `Place`): a drive record whose names are all public to `v2/shared`; every other
drive record to `v2/own`, whoever made it; the checkout's store keeps only what is about this
checkout: certificates ("this request was answered over these files"), their pages and roots,
and `Fetch` records, which vouch for the checkout's `.blaze/v2/sources` folder (fetch.rs:50)
until that folder moves to the home. The leaf table and the profile stay files of the
checkout. Private code is as private as before: both stores are under the user's home, on this
machine; another user on a shared machine has another home and another store, and no write
reaches the network. What an origin may serve is a separate, narrower rule: it is sealed from
`v2/shared` alone (the public names of target.md, "Namespaces"); `v2/own` is never sealed, so
the seam is a folder, not a filter a sealer could forget. Disk: `v2/own` now grows with every
checkout's records (each edit adds packs; equal keys are written once). Nothing collects packs
today: a store merges past 8 packs (`PackStore::merge`), dropping only duplicate keys. Owed,
per target.md "cleanup by generation and last use, no lock": each command appends its read
keys' pack numbers to a last-use log beside the packs; a collector run after the answer
(never waited on) rewrites the coldest packs without the records no command read in N days,
under a byte cap. One project's answer can no longer be its own root set: many checkouts read
one store, so last use, not reachability, decides.

**Seen in reading, a cost not a bug:** a task key whose source is its identity (`CrateScope`)
holds one shape (its read names); the store keeps the first put of a key, so a worktree whose
text reads another file set misses that task's value record each command and reruns it. Never
wrong (a value key is its reads), but two diverging worktrees pay it; the fix is a shape keyed
by its own names, a list of shapes under the task key.

**Written, never compiled (2026-11-16):** store.rs `Place`, `Sharing::place`, `OWN`, `Layered`
with `own`; the chain test's three places and the plant `CheckoutIntoOwn` ("own oracle");
store_worktree_tests.rs: (i) a second worktree runs no task, `machine found N written 0`, its
findings a fresh home's; (ii) a body edit in it reruns one `BodyCheck`, no `CrateScope`, and
the first stays certified with no run; (iii) the planted path in the keys (in the tool every
key holds) is caught: "the second worktree ran".

**The origin sealed from packs (written, never compiled, 2026-11-16).** `PackStore::each_record`
walks a store's packs once a key, sorted; store_http_tests.rs `public_records` reads
`v2/shared` alone through it and `seal_records` seals them (plain segments, `publish`, a
signed head) as CI's writer would. Test `an_origin_sealed_from_the_machine_packs_answers_a_second_machine`:
project A fills the home; the sealed origin is laid out in a folder; every record comes back
byte-equal through `FolderOrigin` and over HTTP from the simulated ranged server (bstd's
`Http`), a liar's flipped byte taken nowhere; project B, empty home, origin named, answers as a
fresh build with fewer runs and "0 not fetched". B's own request reads the origin as a FOLDER:
the request is one blocking lane and the simulation cannot run the server beside it, so HTTP is
proven byte-equal on the same objects instead. Owed: crates/blaze-remote/examples/seal_origin.rs
still reads `.blaze/v2/records/`; a tool over a real home needs the pack reader out of blaze-cli
(a binary) into a library crate, one home for the pack format, then the measured run on
`port27/shared-1116/`.

**TLS under bstd's `Http` (design, no code).** The tree has a sans-IO TLS machine already:
blaze-http `tls.rs` (98 lines), `Session::{Plain, Tls(Box<rustls::ClientConnection>)}` with
`send(plain, out)`, `outgoing(out)`, `received(bytes, plain) -> Result<bool, String>` over
rustls's buffered sans-IO calls (`read_tls`, `process_new_packets`, `reader`, `write_tls`);
config from `roots.rs` (ring provider). Move it, not a second one: bstd `requests` gains a
`tls` feature (rustls with ring) and `requests::Tls`, one type between `Conn` and blaze-h1:
```rust
pub enum Wrap { Plain, Tls(Box<rustls::ClientConnection>) }
impl Wrap {
    pub fn for_url(url: &Url, roots: &Roots) -> Result<Wrap, HttpError>;       // https: SNI = host
    pub fn seal(&mut self, plain: &[u8], wire: &mut Bytes) -> Result<(), HttpError>;
    pub fn open(&mut self, wire: &[u8], plain: &mut Bytes) -> Result<Opened, HttpError>; // Opened { closed: bool }
    pub fn pending(&mut self, wire: &mut Bytes) -> Result<(), HttpError>;      // handshake bytes to send
}
```
`Exchange` sends `pending` until the handshake ends, then `seal`s the request; `read_answer`
feeds each `recv` through `open` before blaze-h1's `Response`. Roots come from `Net::roots`
(`Effect::Roots`), held in `Provider::Over::roots`, turned into a `RootCertStore` once a
command. Errors typed (`HttpKind::Tls { why }`, naming the host), never `String`. The
simulation's server speaks rustls's `ServerConnection` with a test certificate whose root
the sim's `Roots` hold; planted bug: the name check skipped (a certificate for another host
must be refused). Then blaze-http's `tls.rs` and `Session` are deleted with its driver.

## HTTP in the tree (third runner, 2026-11-16: facts by search)

1. bstd `Net` (blaze-std/src/net.rs:216) is blaze-core effects: `connect` (`Effect::Connect`),
   `Conn::send_all`/`recv`/`finish`, `listen`/`accept`, `connect_only` narrowing, `limit` byte
   budgets, `roots` (`Effect::Roots`, :322). Waits end at the `Cx`'s deadline (`cx.until`). The
   simulation runs sockets (blaze-core/src/sim.rs:2966 `Listen`, :2995 `Connect`).
2. blaze-h1 is the one HTTP/1.1 core, sans-IO, `no_std`: `write_request`, `Response` fed in
   pieces under `Limits`, `Response::ranged` (the `Content-Range` check), `Follow`/`Redirects`.
3. Its drivers over bstd `Net`: blaze-h1-net `Client::send` (one request a connection, ranges
   checked, redirects by policy, `Tls` seam with `NoTls`; no caller before this leg) and bstd
   `requests::Http::over` (blaze-std/src/requests.rs:306, behind bstd's `net` feature; one
   request a connection, no redirects, no ranges, `https` refused).
4. blaze-http `Client` (client.rs) is a second machine ABOVE blaze-h1 (client.rs:42, :61 use
   `blaze_h1::Response`, `write_request`): pool, keep-alive, redirects, gzip, proxies, rustls
   sans-IO with roots from `blaze_core::trust::system` (roots.rs:49). It does not call
   `ranged`. Its `driver::Loop` is sans-IO over blaze-core `Effect`s; it leaves bstd's `Io` at
   `Handle::spawn` (driver.rs:620: its own thread and its own `effects::native` driver) and
   `proxy_env.rs:14` (`std::env::var_os`). `h3/mod.rs:88` `std::net::SocketAddr` is a type only.
5. blaze-fetch is sans-IO (`Effect::Http`, fetch.rs:128); its `FetchIo` is blaze-cli
   fetch.rs:418 over `blaze_http::driver::Handle::spawn` (fetch.rs:571), roots by
   `blaze_http::roots::system` (:569), waits in `mailbox::block_on` (:472): off bstd's `Io`.
6. blaze-remote `http.rs`/`http3.rs` (`Bucket` over blaze-http, S3 and HTTP/3) are behind its
   `bucket` feature (lib.rs:108); blaze-cli takes blaze-remote with `default-features = false`:
   not in the binary, not live from the command. The read path's `Origin` had one live form,
   the folder (blaze-cli store.rs `FolderOrigin`).

**The duplicate (for the coordinator; nothing deleted).** One parser already (blaze-h1); the
duplication is three drivers of it: blaze-h1-net (newest, 2026-11-15; on `Net`; checks ranges;
TLS a seam, none written), bstd `Http::over` (on `Net`; a capability with sim/handler
providers; no ranges/redirects), blaze-http `Client`+`driver` (oldest, 2026-11-04; has pool,
gzip, proxies, rustls TLS, a byte-level test suite; no range check; own thread off bstd).
Proposed one home: blaze-http's `Client` machine (it has what a real fetch needs: TLS, pool,
redirects) gains the `ranged` check and is run by ONE driver on bstd `Net` (the `Http::over`
provider), its `Handle::spawn` thread and blaze-h1-net deleted; until then the origin rides
blaze-h1-net, the only driver on `Net` with ranges.

**Built here: the origin over HTTP is an adapter.** blaze-cli store.rs `HttpOrigin` implements
`Origin` with blaze-h1-net `Client::send`: a ranged GET of `<base>/<digest-hex name>` per ask,
`Limits.most_body` the ask's length, redirects `most: 0` (a 3xx is `RedirectsOver`), the
ask's deadline as `cx.until`, `Net::connect_only(origin)`; `--origin http://host:port/prefix`
(`https://` refused by name: blaze-h1-net has `NoTls` only). main.rs passes `io.net()`.

**Handing on (third runner, ~11:00).** Written, compiles (`lanecheck blaze-h1-net`: "check ok
(all targets)"): the adapter above; blaze-h1-net's planted bug (`plant.rs`, the `ranged` call
skipped) and its simulation `tests.rs` (eight servers: honest in tape-drawn pieces, stalls,
closes early, wrong range, wrong length, longer than promised, redirect, 404, each refused by
the name its oracle says). NOT PASSING: "0 passed, 2 failed": in `Io::sim` a `may_overlap`
server task does not run while the main lane blocks (its accept never completes; listening
inside the task gives the client "nothing listens there"). `Client::send` is one blocking call,
so the test needs either the sim's tasks to interleave (ask the effect-layer owner how bstd's
sim runs a task beside a blocked lane) or `send` split into begin/finish as bstd
`Http::over`'s `Exchange` is (its test, blaze-std/src/tests/requests.rs, interleaves by hand).
Same blocker for `crates/blaze-cli/src/store_http_tests.rs` (folder vs HTTP byte-equality and
the lying server), written and NOT wired: its `mod` line was taken out of store.rs to keep
blaze-cli's suite green; put back `#[cfg(test)] #[path = "store_http_tests.rs"] mod
http_tests;` once the server can run. blaze-cli's check: blaze-typeck (another lane) was red
under it; on the green mix: "mixcheck blaze-cli: check ok (all targets) at 10:53; tree
1d76b82d5a". Not done: step 4 (measure: exp2.sh e3/e4, then
the HTTP origin cold run) and the TLS proposal; for TLS the facts: blaze-http's `Client` speaks
rustls sans-IO with roots from `blaze_core::trust::system`; blaze-h1-net has the `Tls` seam
only; bstd `Net::roots` gives `blaze_core::trust::Roots`. Moving blaze-fetch: replace blaze-cli
fetch.rs:418-571 (`Handle::spawn`, `mailbox::block_on`) with the one driver on `Net` once the
coordinator picks the home above.

**Stage one of D63 built (fourth runner, 2026-11-16; supersedes the two paragraphs above where
they differ).** The home is bstd's `Http` (D63), and blaze-h1-net is DELETED (its folder
`crates/blaze-h1-net/` with Cargo.toml, src/{lib,client,plant,tests,tls}.rs, and its
`[workspace.dependencies]` line). bstd `requests.rs` gained, all by blaze-h1's functions:
`Request::range` (sent as `Range`; the answer must be a 206 of exactly it, `Response::ranged`,
checked as the head ends, also before a head the body bound refuses); the body held to
`Wire::body_most` or the range's length, whichever is less, by `Limits` before a byte past it;
a byte after the answer's end is `AfterEnd` (it reads on to the close it asked for);
`Wire::redirects` (`Follow`; `NO_REDIRECTS` refuses with `RedirectsOver { most: 0 }`), a
followed hop is `Finished::Redirected(Exchange)` already sent, so begin/finish holds across
hops; `Wire::wait_most` bounds the connect, the send and each receive, a silent host being
`HttpKind::Silent` (the error's URL names the host). New feature `requests` (blaze-h1 only);
`net` = `requests` + blaze-http/h3. blaze-cli takes `requests`, not blaze-http.
Proof: blaze-std `tests/requests_servers.rs` (honest in tape-cut pieces, stall, early close,
wrong range, wrong length, longer than promised, redirect, 404; each refused by name, nothing
handed on) and its plant `requests::plant` (the range check skipped): `mixcheck test blaze-std
requests` "5 passed, 0 failed" (both new tests and the old three); whole suite "256 passed, 0
failed"; blaze-core "check ok". It found one real bug on its first run: a 404 to a ranged ask
was named `TooBig { most: 2 }` (blaze-h1 refuses a long `Content-Length` before `has_head`),
now `RangeStatus { status: 404 }`. `HttpOrigin` (store.rs) is on bstd's `Http`
(`read_with(.., between)` runs the test's server between send and read) and
`store_http_tests.rs` is wired: `mixcheck test blaze-cli http_tests` "2 passed, 0 failed"
(folder and HTTP byte-equal; a liar refused by the digest chain).

**Stage two, for the next runner (by file and line, read before writing):**
1. TLS as a sans-IO layer between `Conn` and the parser in bstd `requests.rs` (`send` and
   `read_answer`: today `https` is `NoTls`). blaze-http does it with rustls's unbuffered
   sans-IO API inside its `Client` (crates/blaze-http/src/client.rs; roots by
   `blaze_core::trust::system`, roots.rs:49). In bstd the roots come from `Net::roots`
   (net.rs:322, `Effect::Roots`) and are already held in `Provider::Over::roots`. A seam:
   the provider takes a TLS state machine fed bytes in and out; the simulation's server then
   speaks TLS too (test certificates in the simulation's roots).
2. The pool: keep-alive per origin (`close: false`, `Response::reusable`), bounded by count and
   idle time on the `Clock`; `Exchange::finish` hands its connection back to it.
3. Crate fetching onto bstd's `Http`: blaze-cli fetch.rs:418-571 (`FetchIo` over
   `blaze_http::driver::Handle::spawn` :571, roots :569, `mailbox::block_on` :472) becomes
   blaze-fetch's `Effect::Http` performed by `Http::request` on the command's `Net`; a proxy a
   value from main.rs, not `proxy_env.rs:14`. Then delete from crates/blaze-http: `driver.rs`
   (its thread), `proxy_env.rs`, the HTTP/1.1 `Client` core and its tests; HTTP/3 (`h3/`) stays
   behind its feature until it runs on `Net`'s datagrams.
   Measured on this leg's release (greenmix 89688b5872, `exp2.sh e5 .. proto`, load 14.5 to
   15.5): A cold "names resolved for 15 crates", `38 runs`, 2657.583 ms; B cold `10 runs, 34
   store reads (27 found)`, 28.712 ms, 104,399,568 bytes, no names line; B again `0 runs`,
   0.680 ms, profile row `machine found 0 written 0; packs 1 read whole 0`.
4. Not done in this leg: the HTTP origin's cold run. Nothing seals A's records into an origin
   from the pack store (crates/blaze-remote/examples/seal_origin.rs reads the old file store
   `.blaze/v2/records/`), so that measurement needs the sealing tool on packs first, then a
   ranged server under port27/shared-1116/.

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

## End state: what a record's key says about its producer, and a home that stays bounded (architect, 2026-11-16)

**The problem, measured.** Every key holds one `Tool` (blaze-drive key.rs `task_key`,
`cert_key`), made in blaze-cli home.rs `tool` from the running binary's linker id
(main.rs: `home::tool(host.build_id()?.as_bytes())`) chained with every `FORMATS` magic. So
each rebuilt blaze reuses nothing: the owner's home held twelve packs in `v2/shared`, about
215 MB, eight of 23 to 24 MB with equal sizes and different keys, the toolchain's 27 records
written again per binary (compact.rs, module doc). compact.rs `compaction` bounds the number of
packs, not the bytes: an old binary's records stay live by key forever.

### 1. The decision: a key holds its producer, the code of its stage, not the binary

A key is `H(producer(kind), kind, subject, source)`. `producer(kind) = H(drive, stage(kind),
bits)`, where `drive` is the identity of the code that writes every record's envelope and read
list (blaze-drive), `stage(kind)` the identity of the code that makes that kind's payload and
decodes it, and `bits` the two build settings that change what Rust code computes:
debug-assertions and overflow-checks (opt-level changes no result). The certificate and the
answer pages hold `H(every producer)`; the pointer is per (that digest, request shape). A
record's form needs no separate version: its encoder and decoder are code of its stage, so a
form change moves the producer. The `FORMATS` chain in home.rs `tool` goes.

| Kind | Stage code (end state: one root per kind; world (a) roots in brackets) |
|---|---|
| `Units`, `UnitGraph`, `Fetch` | manifests, lock, graph, fetch: [blaze-manifest, blaze-unit-graph, blaze-fetch, after piece 6] |
| `FileItems`, `Surface` | the outline and its digests: [blaze-file-items, parser] |
| `CrateScope`, `ModuleScope`, `Group` (names and solve) | [blaze-world, blaze-scope] |
| `Expansion` | the host side (wasm runner, token codec): [blaze-proc-macro, blaze-expand]; the user's expander stays in the source (`MacroModule` id) |
| `MacroModule` | the wasm compile of a proc-macro crate: [blaze-proc-macro] |
| `Signature`, `ImplBucket` | heads and buckets: [blaze-surface, blaze-typeck] |
| `BodyCheck` (types and borrows, one task) | [blaze-typeck, blaze-flow, blaze-flow-lower] |
| `Code` (build to LIR, emit) | lowering and emit: [the LIR and codegen crates] |
| `Test` | the runner and its wire: [blaze-run] |
| the toolchain's records | not a stage: they are `CrateScope` (and its shapes) records of public crates, keyed by that stage; the pinned toolchain is in their source by its files' content ids, and in `stage` by blaze-toolchain's `PINNED` file |

**(a) Today, cargo builds blaze.** `stage(kind)` is a digest of source text, computed before
cargo runs by a build-time script (tools/stage-ids, piece 2), never by blaze at run time: over
the cargo closure of the kind's root packages (normal and build dependencies, every platform,
from `cargo metadata --locked --offline`, the only cargo run, at build time), each workspace or
vendored package by every regular file under its directory sorted by relative path (target/
and nested packages excluded), each registry package by (name, version, source, checksum), plus
the toolchain pin cargo builds blaze with. `drive` is the same over blaze-drive. The ids reach
the binary through one compile-time variable, `BLAZE_STAGE_IDS`, read by `option_env!` in a new
tiny crate (piece 3); rustc records the variable in dep-info, so cargo rebuilds that crate when
it moves. `bits` are `cfg!(debug_assertions)` and `cfg!(overflow_checks)` in that crate.

A build without the variable (plain `cargo build` in the shared directory) is UNSTAMPED: every
producer is `H(build id, kind)`, as today, and the profile says `identity: whole binary
(unstamped)`. This is chosen at compile time, never at run time on a failure, so it is a mode,
not a fallback (rule 3). Only the build entry (piece 4) sets the variable, and it computes the
ids from a SNAPSHOT it builds (rule 80), never from the directory thirty lanes write in: ids
computed from the tree while a lane writes could describe text cargo did not compile.

Why the closure is complete: a file enters a crate only through an edit of a file already in
it (`mod`, `#[path]`, `include_bytes!` are in the includer; a new dependency is in a
`Cargo.toml`, which is under the package directory); a third-party change moves its checksum.
Coarse but sound: a test-file or README edit in a closure moves the id (a miss, never wrong).

Rejected: **hand-kept form versions under a digest ratchet.** The ratchet must allow "digest
moved, version kept" for a refactor, and that declaration is the failure: a person says nothing
changed, a stale record is read by changed code, the build is wrong. An identity is computed,
never declared. **Whole-binary id for release builds only:** CI would refill every record per
release, and the shared cache would hold one copy of the toolchain per release (the bug above).
**Hashing blaze's sources in const eval or at start:** tens of MB per build or per command
(estimate), and a check at start compares to the disk now, not to what was compiled.

**(b) When blaze builds blaze.** `stage(kind)` is `core::reflect::fn_info(root).code_id`, the
behaviour identity (code-identity.md) of one root function per kind that calls both the stage's
run and its record's decoder; `drive` the same over the envelope and read-list writers. The
compiler computes it while compiling the binary, so it describes exactly the code linked, at
function grain: a bstd edit moves only stages that reach the changed functions. Two
conditions, both gates: a stage root reaches no IO driver (rule 14; IO is a door leaf), and
SIMD or arch-specific helpers in reach (BLAKE3) are `#[blaze::pure(trusted = ..)]` leaves, so
the id is equal on aarch64 macOS, aarch64 Linux and x86-64 (checked: blaze computes each root's
id for the three targets and they must agree). A foreign function is identified by link name
only, so a stage's root id also holds the digest of each native object it links. Pieces 2 to 4
and the variable are then deleted, with a `.clippy.toml` ban on `blaze_stage_id`.

**What makes a stale read impossible, and the test.** A record is found only by its key, and
its key holds the producer of the code that will decode it; a changed stage computes other keys,
so it cannot be handed a record its old code wrote. The drive's simulation (piece 1) gives each
simulated stage an output that is a function of (its producer, its inputs): a stale record then
differs in bytes from a fresh one. Oracle: after a rebuild that changed one producer, every
answer equals a fresh home's in bytes (rule 26), only the changed kind's tasks ran, and every
other kind ran nothing. Planted `Plant::ProducerLeftOut` (one kind's key holds the drive id
only): caught as "a BodyCheck record made by producer A answered producer B". The world (a)
closure is tested causally (rule 23): in a temporary copy, an edit of each file of a closure
moves its stage, an edit outside moves none; planted `omit-lock` (registry checksums left out)
is caught by bumping one checksum.

### 2. The shared cache

Equal on two machines and two builders: in (a) the ids are a function of file bytes, relative
paths, lock entries and two bits; no path, host, time or target is read, so CI on Linux and a
Mac building one commit compute equal ids (piece 4 tests two snapshot slots; CI compares a Linux
and a macOS line). The records themselves are equal only if no stage output depends on the host
(rule 25); that is a separate check this design leans on, not one it proves: CI computes the
toolchain's public records on two hosts and compares keys and bytes (gap row 11). Across an
upgrade: in (a) a stage whose closure did not move keeps its id, and every record of it, local
and in the bucket, still answers; in (b) the same at function grain. CI fills the bucket only
from stamped builds; an unstamped binary's keys never reach it.

### 3. The bound on bytes

**Liveness.** A record is live when its producer belongs to a live binary and some command
used it within 30 days. Two facts, both files beside the packs, neither in a key:
- *The roster*: `<home>/.blaze/v2/roster/<hex16 of H(exe path, build id)>`, one per binary
  that ran: its path, build id, its producers, written (unsynced, by rename) when absent or older
  than a day, so one stat a command. An entry is live while it ran in the last 30 days AND the
  file at its path still carries its build id (Mach-O `LC_UUID`, ELF `NT_GNU_BUILD_ID`, read from
  the file's first 64 KiB). `blaze` and `blaze-old` both installed: both entries live, both
  binaries' records kept. A rebuild replacing `~/.cargo/bin/blaze`: the old entry is dead at once,
  and its producers' records, unless another live entry shares a producer.
- *The use log*: per layer, `used`, fixed 12-byte rows (pack number, row) of records the command
  got from that layer's packs, appended once after the answer, unsynced; a torn tail row is
  dropped. A lost row makes a record look unused: at worst it is collected and made again.

**Each record's producer in its pack.** Pack format 3 (store.rs `FORMAT`): a producer table
(count, 32 bytes each) before the footer; each index row's four spare bytes (store.rs `ROW`) hold
a u16 index into it. A row stays 48 bytes. Format 2 packs are refused and removed at the first
open, as format 1 was: that open alone frees today's 215 MB.

**The collector.** After the answer the command decides from what it already holds (the
indexes, the roster listing, the `collected` stamp's mtime, one stat): collect a layer when its
dead bytes (dead producer, or no use in 30 days and older than 30 days) pass a quarter of it, or
the layer passes its cap, and none ran in the last hour. Then it starts a detached helper,
`blaze collect`, at utility QoS with a 120 s deadline, and exits; the helper takes an
exclusive-create `collecting` stamp (one past its deadline is taken over; a second collector
does not start, nothing waits), streams the survivors into one pack as `PackStore::merge` does,
publishes it, names it in the boot record, then removes the old packs, then rewrites `used`. If
the layer is still over its cap, the packs whose last use is oldest go first.

**Crash safety.** As the merge: publish before remove, nothing synced; a crash keeps both (equal
bytes twice, folded by the duplicate rule) or loses records the driver makes again; never a torn
or wrong record (footer digest, once-a-boot read). A command reading a removed pack keeps its
open file; one that lists after answers a miss. Proof is a DST (rule 43), store/collect_tests.rs
on `blaze_sim` and bstd's crashing `SimWorld`: two binaries' rosters, commands of both
interleaved with a collector, power cut anywhere. Oracle: every get answers its key's bytes or a
miss; after a collection that finished, every record of a live entry's producer used within the
window is present, and no record of a dead producer remains. Planted bugs: `CollectLiveProducer`
(the path check skipped: `blaze-old`'s records removed), `RemoveBeforePublish`, `UseIgnored`.

**The bound.** `shared` ≤ the union over live producers of public records: with the toolchain
at 24 MB (measured, compact.rs doc), L live binaries hold at most L × 24 MB, about 48 MB for
`blaze` and `blaze-old` and less where their stages agree, against 215 MB today and +24 MB a
rebuild. `own` and the checkout store ≤ min(the records of the workspaces used in 30 days under
live producers, the cap); cap = min(16 GiB, 5% of the volume) for `own`, 4 GiB for `shared`
(caps are estimates, sized by the machine, constants in store.rs). Dead bytes are removed within
an hour of the command after the binary that made them is replaced.

### 4. Floors in numbers

| After a rebuild of blaze that... | Today | End state |
|---|---|---|
| changed nothing a stage reaches (docs, tests, another builder of the commit) | all tasks rerun; hello 37 runs, 2,769 ms cold, 27 records, 24 MB written (measured: "Shared across projects", compact.rs doc) | (a) and (b): the certificate hits, 0 tasks, 0 bytes |
| changed `BodyCheck`'s code only | as above; tempo pays its whole first run, about 32 s CPU (drive.md floors, estimate) | toolchain 0 of 27 records, 0 of 24 MB; tempo reruns its bodies only, 31.2 s one thread for 20,000 (measured, tempo-front-9.txt), about 2 to 4 s on 8 to 16 cores (estimate); bytes = the body records (not measured) |
| changed `CrateScope`'s code | as above | 27 records, about 24 MB, rewritten: the floor, as the code changed |
| changed bstd, blaze-ids or blaze-drive | as above | (a): everything, as today; (b): only stages reaching a changed function |

World (a) is coarse where it matters: in this tree since 2026-11-09, of 1,458 commits under
crates/, non-test commits touched blaze-std 42 times, blaze-drive 28, blaze-ids 16 (git log,
counted here); these are in every closure, so a rebuild spanning a day of main is almost always
a full re-make (estimate). World (a) wins inside a lane's session (a typeck lane keeps tempo's
names and scopes across its rebuilds) and across builders of one commit; the floor is world (b).

### 5. Gap (tree blaze-fresh-1115, read 2026-11-16)

| # | Needed | Today | Difference |
|---|---|---|---|
| 1 | a producer per kind in keys | key.rs `task_key(tool, ..)`, `cert_key(tool, ..)`; request.rs `Config.tool` | one `Tool` for all kinds |
| 2 | ids from stage code | home.rs `tool`: build id + `FORMATS` magics; main.rs calls it with `host.build_id()` | whole binary |
| 3 | pointer per (producers, shape) | request.rs `walks_from(last, tool, shape)`; answers.rs `Last.tool` (`BDL1`) | form `BDL2` with the digest |
| 4 | stage code outside the dispatcher | work.rs `run` arms call blaze-cli units.rs, graph.rs, fetch.rs, names.rs (`parts` fills the `UnitGraph` record), macros.rs; the `Surface` record is made in work.rs | moved to stage crates (also rule 31) |
| 5 | a build-time id script and a stamp crate | none | pieces 2 to 4 |
| 6 | a pack names each record's producer | store.rs `FORMAT` 2, `ROW` = key, offset, length, four zero bytes | format 3 |
| 7 | binary liveness | none; compact.rs doc names the gap | roster |
| 8 | own path; another binary's build id | bstd rt/image.rs `build_id` reads the running image only (dyld, dl_iterate_phdr); `uuid_in`, `note_in` parse bytes | `Host::exe`, `build_id_in(file bytes)` |
| 9 | per-record last use | none (this doc, "Shared across worktrees": owed) | `used` log |
| 10 | collection by producer, use and cap | compact.rs `compaction` (pack count, duplicate share); store.rs `merge` drops only duplicates | collector, helper, DST |
| 11 | records equal across hosts, checked | not found by search | CI comparison of public records |
| 12 | code ids on the new layer (world b) | code-identity.md's `fn_info(f).code_id` is the old layer's (hir-ty-identity) | port when blaze builds blaze |

### 6. Pieces (one writer each, none compiles)

1. **PRODUCERS IN THE DRIVE.** CROSS-CRATE BATCH. blaze-drive key.rs (`Producers { drive,
   by_kind: [ContentId; KINDS] }`, `of(kind)`, `digest()`; `task_key(producer, ..)`,
   `cert_key(digest, ..)`), request.rs (`Config.producers`, `walks_from`), answers.rs (`Last`
   form `BDL2`), sim.rs; callers: blaze-cli main.rs, home.rs, store_worktree_tests.rs. Test in
   sim.rs replacing `another_tool_shares_no_record_and_no_certificate`: prints `test
   sim::a_changed_stage_reruns_its_own_records_alone ... ok` and `test
   sim::the_oracle_catches_a_producer_left_out_of_its_key ... ok`. Not: store.rs.
2. **STAGE-ID SCRIPT** (parallel with 1). NEW tools/stage-ids/stage-ids.ts and stages.json (kind
   to root packages; `drive` to blaze-drive); one typed JSON input (rule 42); prints one line of
   hex ids in `Kind` order. Test (bun, temporary copy): prints `stage-ids: blaze-typeck edit moved
   BodyCheck; 15 stood` and `planted omit-lock: caught`. Refuses a closure holding blaze-cli
   once piece 6 is done. Not: any Rust.
3. **STAMP CRATE** (after 1, 2). NEW crates/blaze-stage-id: `const STAMP: Stamp` from
   `option_env!("BLAZE_STAGE_IDS")` (const parse, the count checked against `KINDS + 1`), the two
   bits mixed in; blaze-cli main.rs builds `Producers` from it or, unstamped, from the build
   id; home.rs `tool` deleted with a `.clippy.toml` ban; profile row `identity`. Test: `test
   tests::a_stamp_parses_into_one_producer_per_kind ... ok`. CROSS-CRATE (blaze-cli main.rs).
4. **BUILD ENTRY** (after 2, 3). NEW tools/stage-ids/build.ts: sync the tree into a fixed slot
   snapshot (unchanged files keep their mtimes, so cargo stays incremental), ids from the
   snapshot, `cargo build` there with the variable; refuses when the variable is already set;
   CI's release job and the install scripts call it. Test: one commit in two slots prints
   `build: ids equal across 2 slots`. Not: Rust.
5. **TASK INTERFACE CRATE** (after 1). NEW crates/blaze-task: `Did`, `Made`, `held_str` and the
   task inputs moved from blaze-cli work.rs. CROSS-CRATE (blaze-cli). Test: the existing
   blaze-cli tests, unchanged. Not: stage logic.
6. **STAGE MOVES** (after 5; serialized, one batch at a time, all touch work.rs `run`): (a)
   units.rs, fetch.rs, graph.rs and `names::parts` into their library crates' `task` modules;
   (b) names.rs into blaze-world; (c) macros.rs into blaze-proc-macro; (d) `FileItems`,
   `Surface`, `BodyCheck` arms into blaze-file-items and blaze-typeck. Then a `.clippy.toml`
   `disallowed-methods` entry: blaze-cli makes no `Record`. Test after each: piece 2's script
   prints `stage-ids: blaze-cli edit moved none` for the moved kinds.
7. **PACK FORMAT 3** (after 1). store.rs `seal_records`, `merge`, `read_pack`, chunks.rs: the
   producer table and the row's u16; `put` names the producer from the record's kind
   (blaze-drive `record::open`, as `Sharing::place` does), certificates and pages under
   `digest()`. Test in scale_tests.rs: `test store::scale_tests::a_pack_names_each_records_producer
   ... ok`; a format 2 pack refused and removed. Not: blaze-drive.
8. **ROSTER** (parallel with 7; bstd part first). bstd host.rs `Host::exe`, rt/image.rs
   `build_id_in(&[u8])`; NEW blaze-cli roster.rs: write own entry, `live(cx, home)`. Test (sim):
   `test roster::tests::a_replaced_binary_is_dead_and_an_installed_one_lives ... ok`.
9. **USE LOG** (after 7). store.rs `after_answer` appends each layer's used rows. Test: `test
   store::tests::a_torn_use_log_drops_its_last_row ... ok`.
10. **COLLECTOR** (after 7, 8, 9). NEW store/collect.rs (the rule, survivors, publish then remove);
    compact.rs counts dead-producer bytes as dead; main.rs starts `blaze collect` after the answer
    with its deadline. DST store/collect_tests.rs: `test
    store::collect_tests::a_collection_crashed_anywhere_keeps_live_records ... ok`, and one
    `the_collect_oracle_catches_<plant> ... ok` line for each of the three planted bugs. Not: the
    drive.
11. **WORLD (b)** (when blaze builds blaze and the new layer has code ids). A root per kind, its
    `fn_info(root).code_id` into `Producers`; delete 2 to 4. Test: each root's id equal for the
    three targets, and an edit of one reached function moves exactly its stages.

At once: 1, 2 and the bstd half of 8. Then 3, 5 and 7. Then 4, 6 (serial), 9 and the rest of
8. Then 10. 11 waits for the switch.
