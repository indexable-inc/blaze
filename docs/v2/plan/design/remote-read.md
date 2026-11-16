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
