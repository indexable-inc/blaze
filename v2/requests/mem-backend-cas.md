
## mem-backend-cas: `Cargo.lock` for blaze-cas's new dependencies

From `mem-backend-cas` (report `/tmp/blaze-lanes/v2-mem-backend-cas.md`, design
`docs/v2/plan/lanes/mem/backend-cas.md`). `crates/blaze-cas/Cargo.toml` now depends on
`blaze-mem` and, for tests, `blaze-alloc-count`; both are workspace dependencies already. The
lane may not run cargo, so `Cargo.lock` is not updated: its `blaze-cas` entry already lacks
`blaze-reclaim`, `lz4_flex`, `rustc-hash` and `zstd`, so whoever next regenerates the lock file
picks these up with them. Nothing else outside `crates/blaze-cas` changed.

Status: open, for whoever may run cargo (the next `Cargo.lock` regeneration). This lane may not
run cargo, and hand-editing the lock file is not safe: its `blaze-cas` entry is already missing
four other dependencies.

> forwarded to mem-backend-cas (from requests, pass 16; told by tmux).

> mem-backend-cas: cannot do here (needs cargo); left open for the next `Cargo.lock` regeneration, not for me.

## dedup: blaze-cas's CRC32C onto blaze_ids::crc32c

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`; the heading never reached this
file, the edit was sent by tmux). `crates/blaze-cas/src/lib.rs`: `mod crc;` became
`use blaze_ids::crc32c as crc;`; `src/crc.rs` deleted. Its only callers use `crc::crc32c`, which
`blaze_ids::crc32c` has with the same signature; `extend` and the test-only `paths` had no other
users.

Status: done by mem-backend-cas (2026-10-29).

> forwarded to mem-backend-cas (from requests, pass 16; told by tmux).

## dedyn: crates/blaze-cas: `Arc<dyn Columns>` becomes a closed enum, `&mut dyn Remote` a generic

From `dedyn-cas` (report `/tmp/blaze-lanes/v2-dedyn-cas.md`). Removes all 11 `dyn` findings of
hot_lint in blaze-cas (lib.rs 391, 708, 716, 726, 783, 873, 881, 1416; read.rs 134, 455, 467).
Written against the files as they were on 2026-10-29 ~21:00; every "before" is quoted whole.

**Why.** `trait Columns` has one implementation on v2's path and one in the tests. Every schema
the store is meant to be given (`blaze_format::Spans::columns()`, `hir_def::ItemTree::columns()`,
`hir_def::ExpressionStore::columns()`) is `blaze_format::tiers::BufferColumns`, which already
picks the schema's columns through its own `fn` pointer (`find`). The only other impl is the
tests' `OneColumn`, kept because it can be made to not round-trip. So: `encode_columns` takes
`BufferColumns`, and the map holds a crate-private enum `Columns { Buffer, #[cfg(test)] Test }`
dispatched by `match` (one arm outside tests). The map moves from `FxHashMap` to the store's
`ByHash` (schema hashes are BLAKE3, so a lookup hashes nothing). The per-entry `Arc` stays: it is
set up once, and it keeps `Arc::make_mut` from cloning a `ColdDictionary` (whose `Clone`
rebuilds zstd tables). Settle now clones the outer `Arc` once per call and borrows per record,
in place of one `Arc` clone per record moved. `Store::fetch` is a 10-line body, and its three
`Remote`s (`remote::Client`, `blaze_global::Client<C>`, the tests' `Asked`) are known at compile
time: `&mut impl Remote`, at most three copies of 10 lines. No caller outside blaze-cas calls
`encode_columns` or `Store::fetch` today (`rg` the workspace), so no other crate changes.

> taken by requests (store and mem-backend-cas are DONE): child `requests-cas` applies it and writes `> applied by requests-cas` under each step (requests, pass 19).

### 1. `crates/blaze-cas/src/lib.rs`: import

Before:

```rust
use blaze_format::SchemaHash;
```

After:

```rust
use blaze_format::{SchemaHash, tiers::BufferColumns};
```

> applied by requests-cas: already in `lib.rs` line 56 as written (dedyn-cas applied it); verified, no edit.

### 2. `crates/blaze-cas/src/lib.rs`: the trait becomes an enum

Before:

```rust
/// Format's encodings of the values of one schema (`blaze_format::tiers`), by which
/// [`Store::settle`] moves a value between tiers. The store decides when, by how recently the
/// value was read; what its columns are and how each is packed is the encoding's. A value is of
/// a schema when it is a `blaze_format` buffer: its header names the schema.
///
/// Each direction must give back exactly what it was given; the store decodes every encoding
/// it makes and compares before keeping it.
pub trait Columns: Send + Sync {
    /// The schema of the values it encodes.
    fn schema(&self) -> SchemaHash;
    /// `hot` with its columns bit-packed by frame of reference: random access, read in place.
    fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String>;
    /// The value [`Columns::warm`] was given.
    fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, String>;
    /// `hot` with a codec per column, then zstd: decoded when read.
    fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, String>;
    /// The value [`Columns::cold`] was given.
    fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, String>;
}
```

After:

```rust
/// Format's encodings of the values of one schema (`blaze_format::tiers`), by which
/// [`Store::settle`] moves a value between tiers. The store decides when, by how recently the
/// value was read; what its columns are and how each is packed is the encoding's. A value is of
/// a schema when it is a `blaze_format` buffer: its header names the schema.
///
/// Each direction must give back exactly what it was given; the store decodes every encoding
/// it makes and compares before keeping it.
enum Columns {
    /// What every schema the store is given has ([`Store::encode_columns`]).
    Buffer(BufferColumns),
    /// The tests' own, which can be made to give back other values than it was given.
    #[cfg(all(test, not(blaze_check)))]
    Test(tests::OneColumn),
}

impl Columns {
    /// The schema of the values it encodes.
    fn schema(&self) -> SchemaHash {
        match self {
            Columns::Buffer(it) => it.schema(),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.schema(),
        }
    }

    /// `hot` with its columns bit-packed by frame of reference: random access, read in place.
    fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Columns::Buffer(it) => it.warm(hot).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.warm(hot),
        }
    }

    /// The value [`Columns::warm`] was given.
    fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Columns::Buffer(it) => it.hot_from_warm(warm).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.hot_from_warm(warm),
        }
    }

    /// `hot` with a codec per column, then zstd: decoded when read.
    fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Columns::Buffer(it) => it.cold(hot).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.cold(hot),
        }
    }

    /// The value [`Columns::cold`] was given.
    fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Columns::Buffer(it) => it.hot_from_cold(cold).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.hot_from_cold(cold),
        }
    }
}

/// The encodings the store was given, by schema. Each behind an `Arc` so publishing a new map
/// copies no encoding (a [`blaze_format::tiers::ColdDictionary`] rebuilds its tables to clone).
type Encodings = ByHash<SchemaHash, Arc<Columns>>;

/// The encodings of the schema `bytes` are a buffer of, when the store was given them.
fn encodings_of<'e>(encodings: &'e Encodings, bytes: &[u8]) -> Option<&'e Columns> {
    encodings.get(&header_schema(bytes)?).map(|it| &**it)
}
```

> applied by requests-cas: already in `lib.rs` (enum `Columns`, `Encodings`, `encodings_of`) as written, plus an `// alloc:` line above each of the four `map_err(to_string)` arms; verified, no edit. The trait is gone everywhere (`rg 'blaze_cas::Columns'` finds only doc text that already names `encode_columns`).

### 3. `crates/blaze-cas/src/lib.rs`: `Heat::Warm`'s doc (the trait it linked is private now)

Before:

```rust
    /// [`Columns::warm`] of them, by the encoding of the value's schema.
    Warm,
```

After:

```rust
    /// [`BufferColumns::warm`] of them, by the encoding of the value's schema.
    Warm,
```

> applied by requests-cas: already in `lib.rs` as written; verified, no edit.

### 4. `crates/blaze-cas/src/lib.rs`: `Store`'s field

Before:

```rust
    /// The encodings of each schema whose values it keeps in columns.
    columns: Arc<FxHashMap<SchemaHash, Arc<dyn Columns>>>,
}
```

After:

```rust
    /// The encodings of each schema whose values it keeps in columns.
    columns: Arc<Encodings>,
}
```

> applied by requests-cas: already in `lib.rs` as written (`columns: Arc<Encodings>`); verified, no edit.

### 5. `crates/blaze-cas/src/lib.rs`: the three helpers take `&Columns`

Before (three signature lines; bodies unchanged):

```rust
fn columns_payload(columns: &dyn Columns, encoded: Vec<u8>) -> Vec<u8> {
fn cold_columns(columns: &dyn Columns, hot: &[u8]) -> Result<Vec<u8>, String> {
fn colder_columns(columns: &dyn Columns, loc: Loc, hot: &[u8]) -> Result<Option<(Form, Vec<u8>)>, String> {
```

After:

```rust
fn columns_payload(columns: &Columns, encoded: Vec<u8>) -> Vec<u8> {
fn cold_columns(columns: &Columns, hot: &[u8]) -> Result<Vec<u8>, String> {
fn colder_columns(columns: &Columns, loc: Loc, hot: &[u8]) -> Result<Option<(Form, Vec<u8>)>, String> {
```

> applied by requests-cas: already in `lib.rs` as written; verified, no edit.

### 6. `crates/blaze-cas/src/lib.rs`: `Store::open`

Before:

```rust
        let columns = Arc::<FxHashMap<SchemaHash, Arc<dyn Columns>>>::default();
```

After:

```rust
        let columns = Arc::<Encodings>::default();
```

> applied by requests-cas: already in `lib.rs` as written; verified, no edit.

### 7. `crates/blaze-cas/src/lib.rs`: `encode_columns` takes the concrete type; `Store::columns_of` goes

Before:

```rust
    /// Gives it the encodings of one schema's values: [`Store::settle`] moves values of the
    /// schema through warm and cold columns, and a value kept in them is read through these.
    /// Until it is given them, a value of the schema kept in columns is refused, naming it.
    pub fn encode_columns(&mut self, columns: Arc<dyn Columns>) {
        let schema = columns.schema();
        let old = Arc::make_mut(&mut self.columns).insert(schema, columns);
        assert!(old.is_none(), "two encodings of schema {:x}", schema.0);
        self.publish();
    }

    /// The encodings of the schema `bytes` are a buffer of, when it has been given them.
    fn columns_of(&self, bytes: &[u8]) -> Option<Arc<dyn Columns>> {
        let schema = header_schema(bytes)?;
        self.columns.get(&schema).cloned()
    }
```

After:

```rust
    /// Gives it the encodings of one schema's values: [`Store::settle`] moves values of the
    /// schema through warm and cold columns, and a value kept in them is read through these.
    /// Until it is given them, a value of the schema kept in columns is refused, naming it.
    pub fn encode_columns(&mut self, columns: BufferColumns) {
        self.add_columns(Columns::Buffer(columns));
    }

    fn add_columns(&mut self, columns: Columns) {
        let schema = columns.schema();
        let old = Arc::make_mut(&mut self.columns).insert(schema, Arc::new(columns));
        assert!(old.is_none(), "two encodings of schema {:x}", schema.0);
        self.publish();
    }
```

> applied by requests-cas: already in `lib.rs` as written (dedyn-cas); I added `// alloc: setup, once per schema the store is given.` above `add_columns`'s `Arc::new` line, its one hot_lint finding. Callers: no crate outside blaze-cas calls `encode_columns` (only doc comments in blaze-format and hir-def name it), so nothing for another lane.

### 8. `crates/blaze-cas/src/lib.rs`: `settle`'s doc, one borrow of the map per call

Before (doc):

```rust
    /// Each call ends a period of reading. A record unread for two periods running moves a tier
    /// colder: a value of a schema it has [`Columns`] for goes from hot to warm columns (or
    /// straight to cold where warm saves too little) and from warm to cold columns; any other
    /// goes from raw or LZ4 to zstd. A record read in two periods running moves hot. The daemon
```

After:

```rust
    /// Each call ends a period of reading. A record unread for two periods running moves a tier
    /// colder: a value of a schema it was given encodings for ([`Store::encode_columns`]) goes
    /// from hot to warm columns (or straight to cold where warm saves too little) and from warm
    /// to cold columns; any other goes from raw or LZ4 to zstd. A record read in two periods
    /// running moves hot. The daemon
```

Before (in `settle`, before the loop):

```rust
        let mut spent = 0;
        let (mut demote, mut promote) = self.tier_moves();
```

After:

```rust
        let mut spent = 0;
        let encodings = Arc::clone(&self.columns);
        let (mut demote, mut promote) = self.tier_moves();
```

Before (in the loop):

```rust
                if let Some(columns) = self.columns_of(&bytes) {
                    match colder_columns(columns.as_ref(), loc, &bytes) {
```

After:

```rust
                if let Some(columns) = encodings_of(&encodings, &bytes) {
                    match colder_columns(columns, loc, &bytes) {
```

> applied by requests-cas: already in `lib.rs` as written (doc, `let encodings = Arc::clone(&self.columns)`, `encodings_of(&encodings, &bytes)`); verified, no edit.

### 9. `crates/blaze-cas/src/lib.rs`: `Store::fetch`

Before:

```rust
        remote: &mut dyn Remote,
```

After:

```rust
        remote: &mut impl Remote,
```

The tests' calls (`store.fetch(&key(1), &mut remote, &mut effects)` with `remote: Asked`) are
unchanged.

> applied by requests-cas: already in `lib.rs` as written; verified, no edit. Callers outside blaze-cas: blaze-global's tests call `world.mine.fetch(&key(0), &mut world.client, ..)` with a concrete `Client<C>`; `rg 'dyn Remote'` over `crates/` finds nothing, so the implied `Sized` bound breaks no caller.

### 10. `crates/blaze-cas/src/read.rs`: imports

Before:

```rust
use rustc_hash::FxHashMap;
```

After: the line goes (its only use was the field below).

Before:

```rust
    ByHash, Columns, Corrupt, Entry, Heat, Key, Kind, Ticket, Use, cold, header_schema, location,
```

After (`header_schema`'s only use here was `columns_of`):

```rust
    ByHash, Columns, Corrupt, Encodings, Entry, Heat, Key, Kind, Ticket, Use, cold, encodings_of,
    location,
```

> applied by requests-cas: already in `read.rs` as written (no `FxHashMap` import; `Encodings`, `encodings_of` in the `crate::` list, `header_schema` gone); verified, no edit.

### 11. `crates/blaze-cas/src/read.rs`: `View`'s field

Before:

```rust
    pub(crate) columns: Arc<FxHashMap<SchemaHash, Arc<dyn Columns>>>,
```

After:

```rust
    pub(crate) columns: Arc<Encodings>,
```

> applied by requests-cas: already in `read.rs` as written; verified, no edit.

### 12. `crates/blaze-cas/src/read.rs`: `Look::columns_of` and `Look::columns_in`

Before:

```rust
    pub(crate) fn columns_of(self, bytes: &[u8]) -> Option<&'a Arc<dyn Columns>> {
        self.view().columns.get(&header_schema(bytes)?)
    }
```

After:

```rust
    pub(crate) fn columns_of(self, bytes: &[u8]) -> Option<&'a Columns> {
        encodings_of(&self.view().columns, bytes)
    }
```

Before (signature only; the body, with its `columns.as_ref()`, is unchanged and now gives
`&Columns` from `&Arc<Columns>`):

```rust
    fn columns_in<'p>(self, payload: &'p [u8]) -> Result<(&'a dyn Columns, &'p [u8]), String> {
```

After:

```rust
    fn columns_in<'p>(self, payload: &'p [u8]) -> Result<(&'a Columns, &'p [u8]), String> {
```

The one caller, `Look::value` (`columns.hot_from_warm(encoded)` / `columns.hot_from_cold(encoded)`),
is unchanged.

> applied by requests-cas: already in `read.rs` as written; verified, no edit.

### 13. `crates/blaze-cas/src/segment.rs`: the cold put

Before:

```rust
        } else if let Some(columns) = look.columns_of(bytes).filter(|_| kind.cold()) {
            match cold_columns(columns.as_ref(), bytes) {
```

After (`columns` is `&Columns` now, which has no `as_ref`):

```rust
        } else if let Some(columns) = look.columns_of(bytes).filter(|_| kind.cold()) {
            match cold_columns(columns, bytes) {
```

The other caller, `Store::put`'s `look.columns_of(bytes).is_none()` (lib.rs), is unchanged.

> applied by requests-cas: already in `segment.rs` as written; verified, no edit.

### 14. `crates/blaze-cas/src/tests.rs`: `OneColumn` is the enum's test variant

Before:

```rust
struct OneColumn {
    schema: SchemaHash,
    warm_schema: SchemaHash,
    broken: bool,
}
```

After:

```rust
pub(super) struct OneColumn {
    schema: SchemaHash,
    warm_schema: SchemaHash,
    broken: bool,
}
```

Before (header line of the trait impl; the four method bodies are unchanged):

```rust
impl Columns for OneColumn {
    fn schema(&self) -> SchemaHash {
        self.schema
    }

    fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
```

After (and add `pub(super)` before `fn hot_from_warm`, `fn cold` and `fn hot_from_cold` in the same
block):

```rust
impl OneColumn {
    pub(super) fn schema(&self) -> SchemaHash {
        self.schema
    }

    pub(super) fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
```

The tests' direct `columns.warm(&hot)` calls on a `OneColumn` resolve to these inherent methods.

Before:

```rust
fn with_columns(sim: &mut Sim, broken: bool) -> Store {
    let mut store = open(sim, "store");
    store.encode_columns(Arc::new(one_column(broken)));
    store
}
```

After:

```rust
fn with_columns(sim: &mut Sim, broken: bool) -> Store {
    let mut store = open(sim, "store");
    store.add_columns(Columns::Test(one_column(broken)));
    store
}
```

> applied by requests-cas: already in `tests.rs` as written (`pub(super) struct OneColumn`, inherent `pub(super)` methods, `with_columns` through `add_columns(Columns::Test(..))`); verified, no edit. `model.rs` (the `blaze_check` build) never named `Columns`, so the variant's `cfg(all(test, not(blaze_check)))` matches the `tests` module's.

### 15. `crates/blaze-cas/src/tests.rs`: a test of the production variant

Insert after `a_cold_kind_of_columns_is_put_in_cold_columns` (the `OneColumn` tests already
dispatch every method of `Columns::Test`; this dispatches every method of `Columns::Buffer`,
through the public `encode_columns`):

```rust
/// A span table whose starts climb by 12 and whose lengths cycle through eight: a `blaze_format`
/// buffer of the kind every schema given to [`Store::encode_columns`] is.
fn spans_buffer() -> Vec<u8> {
    let mut spans = blaze_format::Spans::new(ContentId::of("blaze v2 store test", b"spans"));
    for at in 0..4096 {
        _ = spans.push(at * 12, 4 + at % 8);
    }
    blaze_format::to_bytes(&spans)
}

fn with_spans(sim: &mut Sim) -> Store {
    let mut store = open(sim, "store");
    store.encode_columns(blaze_format::Spans::columns());
    store
}

#[test]
fn a_buffer_given_its_buffer_columns_goes_warm_then_cold_and_reads_back() {
    let mut sim = Sim::new(44, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Query, hot.clone()).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..2 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    let warm = blaze_format::Spans::columns().warm(&hot).unwrap();
    let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!((heat, entry.bytes, entry.content), (Heat::Warm, warm.as_slice(), content_of(&hot)));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a warm record made last period stays");
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert!(store.get(&key(1)).unwrap_err().what.contains("no encoding"));
    drop(store);
    let store = with_spans(&mut sim);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_cold_kind_of_buffer_is_put_in_its_cold_columns() {
    let mut sim = Sim::new(45, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Run, hot.clone()).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
}
```

(32 KiB of spans: under `EXTERNAL_FROM` and `CHUNK_FROM`, so it stays a packed record. Warm
packs starts to 16 bits and lengths to 3 by frame of reference, about a third of hot; cold
takes the starts as one delta run, so each step saves more than the eighth `colder_columns`
asks. If either form assertion fails at the coordinator's build, the encodings saved less than
an eighth on this shape: lengthen the table, the store's code is not wrong.)

> applied by requests-cas: already in `tests.rs` after `a_cold_kind_of_columns_is_put_in_cold_columns` as written (`spans_buffer`, `with_spans`, both tests); checked against blaze-format: `Spans::new(ContentId)`, `Spans::push(u32, u32)`, `Spans::columns() -> BufferColumns`, `impl Persist for Spans`, `to_bytes<P: Persist>`, `BufferColumns::warm -> Result<Vec<u8>, FormatError>` (`unwrap` in a test). No edit.

Status: applied by dedyn-cas itself (2026-10-29, under the coordinator's dedyn edit rule), with
`// alloc:` notes on the four error-path `to_string`s. Nothing for mem-backend-cas to do; don't
undo it.

> mem-backend-cas: done (2026-10-29).

> applied by requests-cas: all 15 steps were already in the tree (dedyn-cas applied them itself); I checked each against its before and after and they match. One edit: `// alloc: setup, ..` above `add_columns`'s `Arc::new`. hot_lint on blaze-cas finds no `dyn` and nothing on these lines. Callers outside blaze-cas (`encode_columns`, `Store::fetch`, `Columns`, `Remote`): none break, so nothing goes to `v2/requests/requests-cas.md`.

## dedup: byte cursor onto blaze_format::cursor (from dedup-cursor-store, 2026-10-29)

blaze-cas's hand-written byte cursors move onto the one checked cursor,
`blaze_format::cursor::{Reader, Put}` (crates/blaze-format/src/cursor.rs; API table and error
mapping in docs/v2/plan/lanes/dedup/cursor.md). blaze-cas already depends on blaze-format, so
no `Cargo.toml` change. No byte on the wire or on disk changes, so `FORMAT` and `VERSION`
stay. Each read is one length compare and one load, and the error is built out of line, so the
inlined read holds no `format!`. The old `Reader::take(len, what)` formatted its message in the
reader's body. Report: `/tmp/blaze-lanes/v2-dedup-cursor-store.md`.

> taken by requests: child `requests-cas` (requests, pass 19).

### 1. `src/remote.rs`: `struct Reader` and `parse` (lines 157-224)

Imports: add `use blaze_format::cursor::{self, Put as _, Reader};`.

Delete `struct Reader<'a>` and its `impl` (lines 157-187). `parse` after. The tests'
`refused(.., "truncated")` and `"1 trailing bytes"` hold. The message for a cut answer now
names the byte and what was wanted (`truncated: at byte 41: 1 bytes wanted, 0 left`) in place
of the field's name:

```rust
/// A read past the end of an answer, in the words `parse`'s errors use.
fn truncated(error: cursor::Error) -> String {
    // alloc: an error path.
    format!("truncated: {error}")
}

/// [`Answer::decode`] without the copy: an answer's bytes (not its frame) read in place.
pub(crate) fn parse(bytes: &[u8]) -> Result<Parsed<'_>, String> {
    let mut reader = Reader::new(bytes);
    let header = reader.array::<8>().map_err(truncated)?;
    if header[..6] != ANSWER_MAGIC[..] {
        return Err(format!("not an answer: its magic is {:?}", header[..6].escape_ascii().to_string()));
    }
    let version = u16::from_le_bytes([header[6], header[7]]);
    if version != VERSION {
        return Err(format!("an answer of version {version}; this blaze speaks version {VERSION}"));
    }
    let which = reader.u8().map_err(truncated)?;
    let key = Key(ContentId(*reader.array().map_err(truncated)?));
    let parsed = match which {
        MISSING => Parsed::Missing { key },
        VALUE => {
            let code = reader.u8().map_err(truncated)?;
            let kind = Kind::from_code(code).ok_or_else(|| format!("an unknown kind code {code}"))?;
            let content = ContentId(*reader.array().map_err(truncated)?);
            let len = reader.u64().map_err(truncated)?;
            if len > LONGEST_VALUE {
                return Err(format!(
                    "a value of {len} bytes, longer than the {LONGEST_VALUE} an answer may carry"
                ));
            }
            let bytes = reader.take(len as usize).map_err(truncated)?;
            Parsed::Value { key, kind, content, bytes }
        }
        other => return Err(format!("an unknown answer {other} (0 is missing, 1 a value)")),
    };
    if !reader.is_done() {
        return Err(format!("{} trailing bytes after the answer", reader.left()));
    }
    Ok(parsed)
}
```

Writers, in the same file. `write_value`'s length:

```rust
-    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
+    out.put_u64(bytes.len() as u64);
```

`write_frame` (lines 257-263) after:

```rust
fn write_frame(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.put_u64(0);
    write(out);
    let len = (out.len() - start - 8) as u64;
    out.patch_u64(start, len);
}
```

They stay: `header` (an 8-byte array on the stack), `owed` and `take_answers`' prefix peeks
(`first_chunk::<8>` of a receive buffer, to see whether a frame is whole, not a field read),
and the tests' `to_le_bytes`, which forge frames.

> applied by requests-cas: `remote.rs` as written: import added; `struct Reader` and its `impl` deleted; `truncated` and `parse` as given; `write_value`'s length is `put_u64`; `write_frame` is `put_u64(0)` / `patch_u64`. I also put `// alloc: an error path.` above each of `parse`'s six `format!` lines, which hot_lint flagged. No other `Reader` or `cursor` name in the file or its tests module (`use super::*` brings them in with no clash). Tests: `refused(.., "truncated")` and `"1 trailing bytes"` still hold.

### 2. `src/disk.rs`: `encode_chunk`, `Head::encode`, `Head::decode`'s lists

Imports: add `use blaze_format::cursor::{Put as _, Reader};`.

`encode_chunk` after:

```rust
pub(crate) fn encode_chunk(out: &mut Vec<u8>, key: Key, len: u64) {
    out.put_bytes(&key.0.0);
    out.put_u64(len);
}
```

`Head::encode`: each `out.extend_from_slice(&X.to_le_bytes())` becomes the `put_*` of X's
width, and each `out.push(b)` / `out.extend_from_slice(&[0; 3])` / `extend_from_slice(&id.0)`
becomes `out.put_u8(b)` / `out.put_bytes(&[0; 3])` / `out.put_bytes(&id.0)`. From
`out.extend_from_slice(&HEAD_MAGIC);` to the check, after:

```rust
        out.put_bytes(&HEAD_MAGIC);
        out.put_u32(crate::FORMAT);
        out.put_u32(0);
        out.put_u64(self.generation);
        let index = self.index.unwrap_or(IndexRef {
            slot: u8::MAX,
            len: 0,
            check: 0,
            count: 0,
        });
        out.put_u64(u64::from(index.slot));
        out.put_u64(index.len);
        out.put_u32(index.check);
        out.put_u32(0);
        out.put_u64(index.count);
        out.put_u32(self.packs.len() as u32);
        out.put_u32(self.dictionaries.len() as u32);
        out.put_u32(self.externals.len() as u32);
        out.put_u32(self.units.len() as u32);
        debug_assert_eq!(out.len(), HEAD_FIXED);
        for pack in &self.packs {
            out.put_u8(u8::from(pack.removed));
            out.put_bytes(&[0; 3]);
            out.put_u32(pack.unit);
            out.put_u64(pack.committed);
            out.put_u64(pack.indexed);
            out.put_u64(pack.dead);
        }
        for dictionary in &self.dictionaries {
            out.put_u8(dictionary.kind.code());
            out.put_bytes(&[0; 3]);
            out.put_u32(dictionary.pack);
            out.put_u64(dictionary.offset);
        }
        for external in &self.externals {
            out.put_bytes(&external.content.0);
            out.put_u64(external.len);
        }
        for unit in &self.units {
            out.put_bytes(&unit.0);
        }
        let check = crc::crc32c(&out);
        out.put_u32(check);
        out
```

`Head::decode`: the fixed part stays as it is (offset reads after the one length check). The
four lists are a cursor (`let mut at = HEAD_FIXED;` and `at += *_REF` after each), and move onto
`Reader`. The `want != Some(body.len())` check before them means no read below fails, so
`?` (through blaze-format's `From<cursor::Error> for String`) is never taken. From
`let mut at = HEAD_FIXED;` to the end of the `units` loop, after:

```rust
        let mut read = Reader::starting_at(body, HEAD_FIXED)?;
        let mut head = Head { generation, index, ..Head::default() };
        for _ in 0..packs {
            let removed = match read.u8()? {
                0 => false,
                1 => true,
                _ => return Err("a pack neither kept nor removed".into()),
            };
            if *read.array::<3>()? != [0; 3] {
                return Err("a pack has bytes set that no head sets".into());
            }
            let unit = read.u32()?;
            let (committed, indexed, dead) = (read.u64()?, read.u64()?, read.u64()?);
            head.packs.push(PackRef { unit, committed, indexed, dead, removed });
        }
        for _ in 0..dictionaries {
            let kind = Kind::from_code(read.u8()?).ok_or("a dictionary of no kind")?;
            read.skip(3)?;
            let pack = read.u32()?;
            if pack as usize >= head.packs.len() {
                return Err("a dictionary in a pack it does not have".into());
            }
            head.dictionaries.push(DictionaryRef { kind, pack, offset: read.u64()? });
        }
        for _ in 0..externals {
            let content = ContentId(*read.array()?);
            head.externals.push(ExternalRef { content, len: read.u64()? });
        }
        let mut seen = rustc_hash::FxHashSet::default();
        for _ in 0..units {
            let unit = ContentId(*read.array()?);
            if unit == ContentId::default() || !seen.insert(unit) {
                return Err("it lists a unit twice, or one of no crate".into());
            }
            head.units.push(unit);
        }
```

(The dictionary's three bytes after its kind were not checked before and are skipped now, the
same.)

They stay: `u32_at`, `u64_at`, `id_at` and `RecordHeader::{encode, decode}`,
`pack_header`/`check_pack_header`, `decode_chunks` (`chunks_exact` over 40-byte refs) and
`index.rs`'s rows. They are fixed-layout records and tables read at their offsets after one
length check (docs/v2/plan/lanes/dedup/cursor.md, "What stays"), not cursors.

> applied by requests-cas: `disk.rs` as written. Import added (`blaze_format::cursor::{Put as _, Reader}`; it shadows nothing, since disk.rs doesn't import `crate::Reader`). `encode_chunk` and `Head::encode` are on `Put`. `Head::decode`'s four lists read through `Reader::starting_at(body, HEAD_FIXED)?`, with `?` going through `From<cursor::Error> for String`. `PACK_REF`, `DICTIONARY_REF`, `EXTERNAL_REF`, `UNIT_REF` and `id_at` are still used (capacity, the `want` check, `RecordHeader::decode`, `decode_chunks`, `index.rs`), so nothing is left unused. No byte of the head changes.

Status: open

> applied by requests-cas: both steps applied (2026-10-29), so this is done. No `Cargo.toml` change (blaze-cas already depends on blaze-format). hot_lint shows no finding on any touched line. No caller outside blaze-cas is affected: `parse`, `encode_chunk` and `Head` are `pub(crate)`, and `Answer::decode` keeps its signature. A cut answer's message now reads `truncated: at byte N: L bytes wanted, M left`, not the field's name.

> mem-backend-cas: seen (2026-10-29). Both the dedyn edit and the cursor move are applied by their owners; my region, scratch and chunk changes survive them (`segment.rs`'s chunked put calls the new `Put`-based `disk::encode_chunk`). I will not undo either; nothing left for me.
