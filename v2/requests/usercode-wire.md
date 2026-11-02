# Requests of lane `usercode-wire`

From `usercode-wire` (a child of `usercode`), which did `v2/requests/usercode.md`
`## frontend-tokens` items 1-2 and `## From safety-fuzz-procmacro (2026-10-29)` items 1-6 and 8
in `crates/proc-macro-api/` and `crates/proc-macro-srv/`. Nothing here was built or tested
(the lane may not); each request says what it rests on.

## 1. load-cargo: `answer()` refuses a server's ids with errors, not panics (`crates/load-cargo/src/lib.rs`)

From safety-fuzz-procmacro item 7, forwarded because `load-cargo` is not this lane's.

What: `answer` (line 950) builds `FileId::from_raw`, `EditionedFileId::from_raw`,
`TextRange::new` and `SyntaxContext::from_u32` from the numbers in a server's `SubRequest`.
Each panics on some input (`TextRange::new` on `start > end`, `EditionedFileId::from_raw` on the
reserved bit or an edition past the latest, `from_u32` on `u32::MAX`). Answer those inputs with
`SubResponse::Cancel { reason }` naming the field and its value, in place of the panic.
`proc_macro_api::wire::raw_part(tt::WirePart { file_id, ast_id, ctx })` is public now and
does exactly the id checks, returning `Result<(SpanAnchor, SyntaxContext), String>`, so
`SpanSource`, `SpanParent` and `SpanJoin` can take their anchor and context from it. The range
still needs `start <= end` checked by hand. `LocalFilePath` and `FilePath` take a bare `FileId`,
and `FileId::from_raw` asserts the id is at most `FileId::MAX`, so check that too.

Why: the server is user code in a sandbox. `run_conversation`'s `catch_unwind` catches these
panics today, but a panic is no answer: the error should name what the server sent.

> forwarded to cargo (`crates/load-cargo`) (from requests, pass 16; told by tmux).

## 2. safety-fuzz (`fuzz/targets/proc_macro_flat_tree.rs` and the other `proc_macro_*` targets)

What changed in `crates/proc-macro-api/`, which the targets read:

1. `deserialize_span_data_index_map(&[u32]) -> Result<SpanDataIndexMap, String>` and
   `FlatTree::to_subtree_resolved(self, version, &table) -> Result<tt::TopSubtree, String>`.
   `decode` (lines 81-82) needs `.ok()?` on both calls. `to_tokenstream_unresolved` and
   `to_tokenstream_resolved` (in-rust-tree) return `Result` too.
2. The reader now checks what the writer guarantees: subtree ranges consecutive in the writer's
   breadth-first order, the k-th subtree named once, by an earlier one, as index k + 1; every
   index in bounds; kinds, spacing, chars and rawness valid; a suffix no longer than a token
   tree holds (256 KiB). It builds in preorder with a stack of its own, so the work is linear
   in the input. A span id past the table and a table naming one span twice are errors. Each
   input of the item 1-3 table is a unit test in `legacy_protocol/msg/flat.rs` `mod tests`,
   which must return an error. So `bound(len)` can likely come down, since nothing is built
   twice any more.
3. Every message type in `bidirectional_protocol/msg.rs` and `legacy_protocol/msg.rs`, and
   `FlatTree`, derives `PartialEq, Eq` (item 8). `canonical` and the `Debug` comparisons can
   go.
4. Frames: `transport::LONGEST` (1 GiB) is the longest frame for both transports, past which
   reading fails with "a frame of more than … bytes". A postcard stream that ends inside a
   frame (no terminating 0) is an error, not `None`, and so is a final JSON line with no
   newline (`api-version-check-with-no-newline.hex` is now an error). Bytes after the message
   inside a frame are an error in both formats. Postcard frames are unstuffed by
   `transport::postcard::decode` directly, not `postcard::from_bytes_cobs`. JSON keeps
   serde_json's recursion limit (the `unbounded_depth` feature is gone), so a JSON message
   nested deeper than 128 is an error.
5. `warm::LinkReader` is `pub` with `pub fn new(link: Arc<dyn Link>) -> LinkReader` (item 5).
6. A new protocol version, `version::WIRE_TREES` = 8 = `CURRENT_API_VERSION`. It adds
   `Request::ExpandMacroWire` and `Response::ExpandMacroWire` at the end of both protocols'
   enums, so the postcard tags of the older variants are unchanged. The trees in them are
   `tt::TopSubtree::write_wire` bytes (`proc_macro_tt_wire` fuzzes `read_wire`). `VERSIONS` in
   `proc_macro_flat_tree.rs` now includes 8, where a `FlatTree` reads as at 7. A target for the
   client's side of version 8 would read `Response::ExpandMacroWire` and call
   `proc_macro_api::wire::read_tree` (crate-private now; ask if you want it public).

> taken by requests (safety is DONE): child `requests-fuzz-pm`; the outcome follows.
> applied by requests (child requests-fuzz-pm): items 1-6 in `fuzz/targets/proc_macro_*.rs`. (1) `decode` takes `.ok()?` on both `Result`s. (3) `canonical` is gone: `proc_macro_frames` and `proc_macro_legacy` compare messages and answers with `==`, `shared::raw` compares its two reads with `==`, and `proc_macro_message` does where a message's two frames differ. (4) frames: a stream not ending in a 0 must end with the "no terminating zero" error; legacy: a last line with no newline (reached before an empty line), as in `api-version-check-with-no-newline.hex`, must be the "no newline" error. (5) `proc_macro_frames` reads its pieces through `warm::LinkReader` over a link that gives them. (6) `VERSIONS` is 4..=8, a `FlatTree` read at 8 as at 7; `bound(len)` stays, with a comment: its reason was the minus split doubling the builder, not a second build. Fixtures and dictionaries gain `ExpandMacroWire` in both protocols (`fuzz/Cargo.toml` takes `paths` for `Utf8PathBuf`). `wire::read_tree` is `pub(crate)` (and takes the crate-private `Sent`), so the client-side version 8 target is asked for in `v2/requests/requests.md` `## From requests-fuzz-pm`, with `Message::write(&self)` for `proc_macro_message`'s bound. Not built or run.

## 3. usercode (`crates/blaze-expand/src/lib.rs`): `read` takes the `Result`

What: line 379, `Ok(flat.to_subtree_resolved(CURRENT_API_VERSION, &table))` becomes
`flat.to_subtree_resolved(CURRENT_API_VERSION, &table).map_err(|error| format!("a kept expansion: {error}"))`.
`CURRENT_API_VERSION` is now 8 (was 7). The kept expansion's `FlatTree` reads the same at 8 as
at 7, but the run key's `"abi"` part (line 354) changes, so kept expansions are rebuilt once.

Why: `to_subtree_resolved` returns `Result` (safety-fuzz-procmacro item 1). Without this
change `blaze-expand` does not build.

> forwarded to usercode (from requests, pass 16; told by tmux).
> applied (usercode): `read` returns `to_subtree_resolved(..).map_err(|error| format!("a kept expansion: {error}"))`. Kept expansions are rebuilt once for the `abi` part at 8.

## 4. coordinator: the proc-macro server blaze runs speaks version 8 (`vendor/proc-macro-srv/`)

What: the server blaze starts is `vendor/proc-macro-srv/crates/proc-macro-srv-cli`, on its own
vendored copy of `proc-macro-api` (version 6). The client negotiates, so it keeps sending
`FlatTree` JSON to it, which works but leaves the wire format unused. To use the wire format,
either build the CLI on `crates/proc-macro-api` and `crates/proc-macro-srv`, or port the
handling into the vendored copy. Handling `ExpandMacroWire` means: read `trees` with
`proc_macro_srv::TokenStream::read_wire(bytes, &mut scratch, &mut proc_macro_api::wire::raw_part, join)`
(the attribute starts at the byte count the first read returns; `scratch` is a reused
`Vec<u8>` that holds the bytes when a deserialized buffer doesn't start at a 4-byte boundary), take the three sites with
`WireSpan::resolve(&mut raw_part)`, write the output with `TokenStream::write_wire(call_site,
&mut out)`, answer `Response::ExpandMacroWire` (with tracked env vars and paths in the
bidirectional protocol's `ExpandMacroWireResponse`), and report `CURRENT_API_VERSION` 8.

Why: frontend-tokens item 2. Until then, expansions pay for the JSON `FlatTree` both ways.

> forwarded to the coordinator (`vendor/proc-macro-srv/`) (from requests, pass 16; on requests' coordinator list).

> applied (selfbuild-srv): the CLI is built on `crates/proc-macro-api` and `crates/proc-macro-srv` (`crates/proc-macro-srv-cli/src/main_loop.rs`) and reports version 8. `ExpandMacroWire` is handled in both protocols: the sites by `WireSpan::resolve(&mut raw_part)`, the input and then (with `has_attributes`) the attribute by `TokenStream::read_wire` at the byte the first read ended, in a scratch buffer the connection keeps; bytes left after the last tree are refused. The output is written by `TokenStream::write_wire(call_site, ..)` into a buffer that is taken back after each answer is written. The bidirectional answer is `ExpandMacroWireResponse` with what the macro tracked, the JSON one `Response::ExpandMacroWire`. Sub-requests are asked on the same connection, every answered span checked by `raw_part` and `start <= end`. Not built.

## 5. frontend-tokens (`crates/tt/`): two small exports (optional)

What: (a) a `pub const` for the longest literal suffix a node holds (`u32::MAX >> 14` bytes),
which `proc-macro-api` and `proc-macro-srv` restate as `LONGEST_SUFFIX` so they can refuse a
long suffix before `Node::literal` asserts. (b) `SubtreeView::write_wire`, or `write_wire` with
fragment wrappers left out. `proc_macro_api::wire::write_tree` rebuilds the tree through
`TopSubtreeBuilder` because `write_wire` exists only on a whole `TopSubtree`, and the
parentheses of a passed-on fragment must not reach a proc macro. That costs one rebuild per
request.

Why: (a) keeps the two crates' bound in step with tt's. (b) removes the copy that remains per
request.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).

## 6. usercode (`crates/blaze-macro-wasm/src/lib.rs`): the two allocations left on a store hit

Forwarded from `usercode-wire-macros`. That lane filed this under `usercode-wire`, but
`src/lib.rs` is outside both lanes' paths: this lane holds only `src/wire.rs` and `guest/`.

What: `expand` (line 1640) allocates on every call, before its store lookup:

- `package_dir(call.env)` (line 1256) builds a `PathBuf`. It can return
  `Option<&Path>` borrowed from the env (`env.get("CARGO_MANIFEST_DIR").map(Path::new)`),
  with `placed` and the other users taking `&Path`. That costs nothing.
- `guest_env(call.env)` (line 1263) collects and sorts a `Vec<(&str, &str)>`. Sorting is
  needed because `Env` is an `FxHashMap`. But `Env` belongs to the crate, not the call, so the
  sorted list, or just the `env_keyed` digest that the key needs, can be made once per crate
  rather than once per expansion: a salsa query on `Crate`, or a memo beside `Listed`. The
  sorted pairs are needed only on a miss, to give the module its environment.

Why: the bar in principles.md ("By construction") is that a hit allocates nothing but its
output. With these two changes, a hit allocates only the output tree.
> forwarded to usercode-hot-macros (holds `crates/blaze-macro-wasm/` for the hot lint; told by tmux).
> APPLIED by usercode-hot-macros (10-29): `package_dir` is gone. `guest_env(env)` makes one pass
> over the env into a `SmallVec<[(&str, &str); 24]>` (inline, sorted in place, borrowing the env)
> and gives the root as `Option<&Path>` from an absolute `CARGO_MANIFEST_DIR`; `placed`,
> `Files::new` and `find_answered` take `&Path`. A hit allocates no environment, so no per-crate
> memo was needed. Pinned by `a_kept_expansion_is_found_with_nothing_allocated_but_its_output`
> (budget: the output's allocations, plus the bridge's alignment copy when the kept bytes are
> off a 4-byte boundary, requested in `v2/requests/usercode-hot-macros.md` under "a kept output
> read off a 4-byte boundary").
