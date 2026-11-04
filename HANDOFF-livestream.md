# Handoff: live feed as one long-lived Arrow IPC stream per table (branch pd-livestream)

Written without building, checking or running anything. Nothing committed.

## Files changed

- `crates/blaze-profile/src/live.rs`: the change itself, the new top doc and the tests.
- `crates/blaze-profile/Cargo.toml`, root `Cargo.toml` (`[workspace.dependencies]`), `Cargo.lock`:
  a new direct dependency on `arrow-buffer` 60.1.0 (already in the lock through arrow-array, and
  patched to `vendor/arrow-buffer`). `StreamDecoder::decode` takes an `arrow_buffer::Buffer`. The
  lock change is one line, `"arrow-buffer"` added to blaze-profile's dependency list. Check that
  blaze accepts it.
- `crates/blaze-cargo/src/cli/observe.rs` (`blaze debug live`): one `live::Decoder` per table
  that lives for the whole subscription. It used to call `live::decode` once per frame.
  `schema` was added to the `blaze_profile` import.
- `crates/blaze-explore/src/server.rs`: the doc of the live stream, and `server::tests` now
  decodes the first live frame with a `live::Decoder` for the table named in its header.
- `crates/blaze-explore/src/frame.rs`, `crates/blaze-api/src/lib.rs` (`LiveFrame`): docs only.
- `docs/profiling.md` ("The live feed") and `docs/v2/plan/lanes/observe/explore.md` ("The live
  stream", plus the memory bound): updated to describe the new wire.
- Not changed: the daemon's `serve/observe.rs`. It still uses `Feed`/`Frame` the same way and
  moves each frame's bytes into `Response::Live`. `blaze-ui/src/api.rs` only prints byte counts.

## Design

- **`live::Encoder`** (one per table per subscriber, held in `Feed`) wraps arrow-ipc 60.1's
  `StreamEncoder` with zstd and `DictionaryHandling::Delta`.
  - arrow only sends a delta when the new dictionary values have the old ones as a prefix. So
    each top-level `Dictionary(UInt32, Utf8)` column is re-keyed onto the stream's own `Dict`:
    a `blaze_hash::HashMap<Box<str>, u32>` plus a `StringBuilder`, with values added in the
    order rows first name them. Only values that rows actually reference are added.
  - The values array is rebuilt (`finish_cloned`) only when a value is new. Otherwise the same
    `Arc` goes out, the tracker's `ptr_eq` fast path fires, and no dictionary message is sent.
  - A nested dictionary column (List/Struct/Map) is refused with an error naming the table and
    column. The live tables have none.
  - The schema goes out lazily, with the first batch that has rows. **A table with no rows
    writes nothing, so a table's bytes are empty exactly when its rows are 0.** This keeps the
    explorer's existing `Emit::Nothing` skip correct.
- **Reset:** `Cap::LIVE` is 65,536 values or 1 MiB of value text per table, summed over its
  dictionary columns.
  - Before a batch is encoded, a full stream is dropped. Its frame's bytes start with `END` (the
    IPC end-of-stream marker `FFFFFFFF 00000000`), then a fresh schema and full dictionaries.
  - A stream can go over the cap by at most one batch's new values.
  - The `END` is in-band, so the wire format stays standard concatenated IPC streams. No field
    was added to `LiveFrame` or to the explorer's header.
- **`live::Decoder`** (one per table on the client) keeps a `StreamDecoder` across frames.
  - If a frame starts with `END`, it feeds the marker to the old decoder and calls `finish()`
    (which fails if that stream stopped mid-message), then starts a new `StreamDecoder` on the
    rest.
  - After each frame it calls `finish()` to check that the frame ended on a message boundary.
  - An `END` arriving before any stream began is a typed error naming the table.
  - Errors name the table and the stream number.
- **Errors keep the stream consistent:**
  - If `Encoder::encode` fails, it drops its stream and clears its bytes.
  - If `Feed::next` fails partway, it `undo()`es the tables already encoded in that frame. Undo
    drops the stream and restores `begun` to its value before that encode, so a stream the
    client never received does not start with `END`. Then it clears all three tables.
  - This matters for the explorer, which sends an error frame and keeps going.
- **Determinism:** dictionary keys come in order of first reference by row, maps are only looked
  up and never iterated, and zstd is deterministic. A test checks same rows in, same bytes out.
- `live::decode` (one-shot, whole stream) stays for `views` and `blaze debug sql --live`
  answers. Its doc now says it is not for the live feed. The old public `live::ipc` (one fresh
  stream per frame) is removed; nothing outside live.rs used it.

## Risks

- **The ix.dev explore page (not in this repo) must change.** It needs a long-lived IPC reader
  per table, must feed it every frame with rows in order, and must start a fresh reader after
  the `END` marker. apache-arrow JS `RecordBatchStreamReader` with `autoDestroy: false` reads
  concatenated streams. A page that still runs `tableFromIPC` on each frame will break from the
  second frame on, because there is no schema in it. Nothing in the hello signals the change;
  consider a `live_ipc: "stream"` hello field or a `schema::VERSION`-style bump if the page
  needs to tell old from new.
- Stream continuity now rests on the daemon delivering every `Response::Live` to the client, in
  order. Today it does: a subscriber that is waiting is skipped before `feed.next`, so nothing
  is encoded and then dropped. Any future path that drops a reply after encoding has to call
  undo or reset the stream.
- `Feed` now holds three `StreamEncoder`s. Each has a zstd compressor context that it reuses,
  so per-subscriber memory goes up by about 3 zstd CCtx. The explorer moves `Feed` into
  `spawn_blocking`, so `Feed: Send` is required. That depends on the workspace's zstd
  `bulk::Compressor` being `Send`. It is upstream; check it under the zstd patch.
- The `END` detection assumes every frame starts on a message boundary. The encoder guarantees
  this, and the decoder checks it with `finish()` after each frame.
- The decoder concatenates the existing dictionary with each delta (arrow's
  `update_dictionaries`). That is O(dictionary) per frame that has new names, bounded by the
  cap.

## Tests added (in `live.rs`)

- `a_stream_sends_each_name_once_and_decodes_to_its_rows`
  - Encodes four `open` frames (new names, the same names again, zero rows, two new names plus
    repeats) and decodes each. Rows match what went in, with dictionary columns compared by
    string.
  - Parses the IPC messages of each frame:
    - frame 1 is schema plus 6 full dictionary values;
    - frame 2 is the batch alone;
    - frame 3 is empty bytes;
    - frame 4 is two 1-value deltas, with no schema and no `END`.
  - Checks that two encoders given the same frames produce identical bytes.
- `a_full_dictionary_ends_the_stream_and_the_decoder_follows`: with `Cap { values: 6 }`, frames 2
  and 3 each start with `END` and then a schema, with fresh full dictionaries. The decoder
  follows (`streams()` reaches 3 on both sides), and a fresh decoder refuses a frame that starts
  with `END`, naming `open`.
- `a_frame_not_sent_is_taken_back`: undo after a frame that was not delivered, both mid-stream
  and for the first stream ever.
- Changed: `the_view_shows_...` uses an Encoder/Decoder round trip. `a_feed_sends_what_closed_once`
  keeps one `Decoder` across frames and checks that bytes are non-empty exactly when rows are
  non-zero. `a_frame_round_trips_through_ipc` is gone, replaced by the first new test.

## What the main agent should check

1. `blaze check` across blaze-profile, blaze-cargo, blaze-explore and blaze-api. Likely compile
   snags:
   - borrows in `Decoder::decode` (closures over `stream` and then `self.error`);
   - `Buffer::from_slice_ref(END)` type inference;
   - `rest.strip_prefix(&END)`;
   - the `[&schema::SPANS, ..].map(live::Decoder::new)` in the CLI.
2. That blaze-lints is clean on the `// alloc:` / `// clone:` annotations: implicit_allocator on
   `vec![UNSEEN; ..]`, `columns().to_vec()`, `value.into()` to `Box<str>`, and
   `StringBuilder::new`.
3. `blaze test -p blaze-profile live::` and `-p blaze-explore server::tests`.
4. The explore page owner needs to hear about the wire change (Risks, first point).
