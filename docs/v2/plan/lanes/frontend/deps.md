---
status: plan
summary: blaze v2 reads dependency crates from images of their def map, lang items and signatures keyed by every input, parses only the bodies codegen or const eval asks for, and reparses and re-lowers only the items an edit touches (frontend.md items 5 and 14).
date: 2026-10-29
---
# blaze v2: dependencies by signature, and per-item reparse (frontend-deps)

Design notes of lane `frontend-deps` for items 5 and 14 of
[the front end's design](../../design/frontend.md), as written on 2026-10-29. Nothing was built
or run: every claim below is what the code is written to do, and each test named is what proves
it once it runs.

## What is matched

- **Item 5, dependencies.** The benchmark is rust-analyzer's and rustc's treatment of
  dependencies: rustc reads a dependency's `.rmeta` (its def map, signatures and MIR of
  generic and `#[inline]` items) and never its source; rust-analyzer parses every file of every
  dependency and lowers it in full. blaze v2 matches rustc's "a dependency costs a read of its
  metadata" and keeps rust-analyzer's laziness: the image is read in place, a signature is
  decoded only when asked, and a body is parsed only when codegen or const eval asks for it,
  one function at a time (outline parse, #1294). Measured by the time to first diagnostic of a
  workspace on a warm store against rustc's `cargo check` with a warm `target/` (#1293).
- **Item 14, edits.** The benchmark is rust-analyzer's incremental reparse
  (`syntax::parsing::reparsing`, a token or a `{}` block relexed) and Roslyn's
  red-green incremental parse: an edit costs work in the size of what it touches, not of the
  file. blaze v2 reparses the smallest covering item or block (`Reparser`) and lowers item trees
  per item, so items an edit does not touch keep their memo.

## Item 5: dependency images (`crates/hir-def/src/deps/`)

- **Which crates.** `deps::eligible`: a crate whose origin is not local and none of whose
  transitive dependencies is local. Workspace members, and anything built on one, resolve from
  source; `blaze check` of the workspace checks them fully.
- **Key** (`deps/key.rs`), portable: no absolute path of the machine is hashed, so two
  checkouts at different paths, on one machine or two, have the same keys. It hashes:
  - `build` (the blaze reading, from the driver) and the encodings' versions (`PRODUCER`: image
    schema, `frozen::ENCODING`, `signatures::SCHEMA`, the reads' domain);
  - every field of the crate's `BuiltCrateData` and `ExtraCrateData`, destructured with no `..`,
    so a field added to either fails to compile until the key covers it. These are the root
    path, edition, dependencies (name, flags and their keys), origin, crate attributes, target
    kind and crate types, the proc-macro cwd, version, display name, potential cfg, workspace
    data (toolchain, target layout) and cfg, and the crate's variables sorted;
  - the source root's files, each as its path and its text's content ID, sorted (`OUT_DIR` files
    included, since the v2 load puts them there);
  - for a proc-macro crate, each macro's name, kind, disabled flag and library content ID
    (`ProcMacroExpander::code`). A proc-macro crate with a macro of unknown code is not
    eligible, and neither is any crate built on one.

  It is computed here rather than from base-db's `Crate::content` and source-root content IDs,
  which name files by absolute path. Paths and variable values are written relative
  (`Base`): the package directory (`CARGO_MANIFEST_DIR`, else the root file's directory)
  becomes `\0PKG`, `OUT_DIR` becomes `\0OUT_DIR`, and any other absolute value of a variable
  (`CARGO`, `RUSTC`, a home directory) becomes `\0ABSOLUTE`. An image whose parts hold one of
  those paths, at least 8 bytes long, would depend on what the key left out, so it is refused
  (`Refused::MachinePath`).
- **Reads** (`deps/reads.rs`): what a crate read that is known only once it is resolved, kept
  as `blaze_usercode::index` keeps a run's shapes. It is two lists:
  - the process variables its `env!` and `option_env!` read that its own variables lack (from
    base-db's `Files::env_read` log, which notes every such expansion, nested ones too);
  - each proc-macro expansion's `ExpansionReads::Traced { shape, values }`, as its server
    reported it.

  The names (`Reads::encode`) go in the first free of 16 slots under the key
  (`reads::slot(K, n)`). The image goes under `reads::image_key(K, names, values)`, where
  `values` hashes the variables' values and each expansion's values hash. A crate that read
  nothing of the kind has its image under `K`, which is one get. Serving asks `K`, then each
  slot in turn: it decodes the names and works out their values now (the database's process
  variables, `ImageStore::reads_now` for each shape with the crate's variables), then gets the
  image they name. A value that changed is a miss (`Served::stale`); the crate is resolved and
  imaged again under the same slot, never served from an older image.
- **Bodies are filed by their own reads** (`deps/filed.rs`). `Reads::of` walks the macro
  calls of module scopes and signatures, never those inside bodies, because the image holds no
  body. A value made from bodies is filed as an image is, through three functions:
  - `checked_reads(db, krate)` gives the reads of every body's macro calls, and of the
    signatures and bodies of the items in their blocks, after the check. For a crate read from
    an image it leaves out the module and signature calls: the image's key covers those, and
    asking for them would expand what serving skips. So such a value's key must cover the image.
  - `file_by_reads` keeps those reads in a slot of the value's key and gives the key the value
    goes under.
  - `find_by_reads` asks the key, then each slot with its reads' values now.

  `serve` and `store` go through the same `search` and `file_by_reads`. The users are check
  outcomes (`blaze_diagnostics::cache`, `hir_ty::check_cache`, by `Stored::filed` or
  `Image::key`). Constant values (`hir_ty::consteval::deps`, by `deps::const_key`) still have
  to add their bodies' reads.
- **What the key and reads do not cover is refused** (`deps::store`, `Refused`):
  - an expansion whose reads are not known (`ExpansionReads::Unknown`: untraced, it asked the
    database, it read the clock, or its macro keeps a session). A wasm proc macro reports
    nothing, so it is always unknown: its expansion is keyed by the database's answers to its
    span questions, which `reads_now` can't work out again. Native proc macros, the default,
    are covered;
  - two expansions that read the same names and found other values;
  - a def map that uses the session's macros;
  - a module file or `include!` outside the crate's source root;
  - a part holding a machine path.

  Such a crate resolves from source every build, and the driver shows why under `-v`.
- **Image** (`deps/image.rs`). One `blaze_format` buffer: a `Root { key, parts }` and three
  parts (`Part::DefMap`, `LangItems`, `Signatures`), each 16-aligned so the signatures part,
  a buffer itself, is opened in place. `ImageRef::open` checks the header and schema, that the
  root's key is the key it was found by, and each part's bounds, tag order and alignment; the
  store checks the content hash on every read.
- **Serve** (`deps/serve.rs`). `deps::serve(db, crates, build, store, mode)` keys the crates,
  reads each image the store has, checks the def map part names only files the crate has
  (`frozen::unreadable`), and gives the database the def maps and lang items
  (`frozen::set`, `set_lang_items`) and the images (`DepImages`, read a crate at a time
  through the tracked `deps::image_of`). An image that does not read under its own key is an
  error naming the crate: never a crate resolved instead. `serve` adds to what earlier calls
  served, so the daemon serves crates as their build scripts finish:
  - a crate served before under the same key and mode is kept and not fetched again;
  - one whose key changed is fetched again, and loses its image when none is found.
- **Store.** `deps::store(db, served, store)` writes the image of each eligible crate not
  served, after the refusals; a store that keeps other bytes under the key is an error.
- **In place.** `ImageStore::get` returns an `ImageBuf` (`Arc<dyn Held>`, where `Held` lends
  the bytes). The driver gives `blaze_cas::Reader::pinned`, a verified range of the store's
  mapping at a 16-byte boundary, so an image is never copied. Its parts (`PartBytes::In`)
  borrow it: the frozen def maps and lang items (`Frozen.bytes`), and signature stores
  (`ExpressionStoreRef::decode_in`, whose columns are `Col::borrowed`).
- **Public keys** (for `blaze_global::Rule::image`). `deps::key_inputs` gives each key with
  its build, the closure of crates it covers and their `OUT_DIR`s, from the walk that made
  it. `Reads::names`, `deps::slot` and `deps::image_key` say which derived key a driver asks
  about.
- **Global cache.** `ReadThrough { local, global }` is an `ImageStore` that asks the local
  store, then the global one, and keeps what the global one gave locally. Puts and `reads_now`
  go to the local store. Portable keys are what make a global image usable.
- **Modes.** `Mode::Serve` reads the image; `Mode::Check` resolves as well and records each
  difference; `Mode::Verify` re-encodes what was read and records a difference from the bytes.

- **Portable ids for other parts.** `deps::{PortableIds, PortableId, IdKind, body_key,
  BODY_KEY_DOMAIN}` (`deps/signatures/ids.rs`) let another part of an image
  (`hir_ty::check_cache`) name a crate's items through the signatures part's table and file a
  body under its item's key.

## Item 14: per-item reparse and lowering (lane `frontend-deps-reparse`)

Item 14 has two parts:
- **The parse** is db's D8: `base-db/src/reparse.rs` keeps each file's last parse and
  reparses it with `syntax::Parse::reparse_onto`, checked against a whole parse in debug
  builds.
- **Lowering**: `hir-def/src/item_tree/items.rs` keeps each file's last tree and re-lowers
  only the items an edit touched, assembled byte-equal to a whole-file lowering.

frontend-deps-reparse's own parse cache, `syntax::SourceFile::parse_file` over a 128-slot
table (`syntax/src/parsing/kept.rs`), had no caller once db applied D8 instead. It and its
tests were deleted on 2026-10-29. `Parse::reparse_onto` and `Parse::reparse_to`, with their
tests, stay in `crates/syntax`. Notes: [reparse.md](reparse.md).

## Tests and what each proves

In `crates/hir-def/src/deps/tests.rs`:

- `only_crates_outside_the_workspace_are_eligible`, `a_library_on_a_member_is_not_eligible`:
  workspace crates are never served.
- `keys_are_the_same_in_every_database`, `a_key_covers_every_input`: a key is deterministic and
  changes with a non-root file, a dependency's text, cfg, edition and `build`, and not with a
  dependent's or a member's text.
- `an_image_reads_back_its_parts`, `an_image_under_another_key_does_not_read`,
  `damaged_images_read_within_bounds_or_not_at_all`: the format round-trips and refuses
  misfiled or damaged buffers without panicking.
- `every_dependency_is_stored_and_served`, `a_served_dependency_resolves_as_a_fresh_one`
  (each mode), `an_unresolved_import_of_a_dependency_is_still_reported`: a database that reads
  every dependency from its image has the same def maps, diagnostics and lang items, byte for
  byte in the frozen encoding, as one that resolved them; check and verify modes record no
  difference.
- `an_edit_to_a_dependency_misses_its_image_and_its_dependents`,
  `an_image_written_under_another_key_is_an_error`,
  `a_crate_with_no_outside_input_is_not_refused`, `serving_asks_the_store_once_a_crate`.
- Proc macros, with a test expander (`Reading`) that reports its library and its reads:
  - `a_crate_with_traced_expansions_resolves_from_its_image_as_a_fresh_one` (each mode) is the
    differential test. A crate of attribute expansions is filed by its reads and gives the
    same def map, diagnostics and signatures, byte for byte, as resolving it.
  - `a_changed_read_misses_the_image_and_the_next_one_is_found`: a changed value is stale,
    and the new image shares the slot.
  - `a_key_covers_the_library_of_its_macros`: a rebuilt library changes the key.
  - `a_crate_with_an_expansion_of_unknown_reads_is_refused`: the fixture's expanders, which
    report nothing, are refused, and their users are not eligible.
- `process_variables_read_file_the_image_by_their_values`,
  `reads_read_back_and_damaged_ones_do_not_panic`: the reads' values and encoding.
- `keys_hold_no_path_of_the_checkout`: two checkouts with different package directories,
  `OUT_DIR`s and tool paths have equal keys, while a changed text, a moved file or a renamed
  variable gives another. `an_image_holding_a_path_of_the_checkout_is_refused` covers
  `Base`.
- `the_global_cache_is_read_through_the_local_store`: the global store is asked once per
  crate, and the next session asks it nothing.
- Outline signatures against full-parse signatures (fixtures and a proptest) and served
  signatures against computed ones are `frontend-deps-sigs`'s, in `deps/signatures/`.

## Left, with who

- db: the signature hooks `frontend-deps-sigs` sent (notes: [signatures.md](signatures.md)).
- cargo-daemon: the `ImageStore` adapter over `blaze-cas`, `serve` once inputs are final,
  `store` after the build, a daily `Mode::Check` run (`v2/requests/cargo.md`).
- db-expand (`v2/requests/db-expand.md`): `ProcMacroExpander::code`, `ExpansionReads`,
  `report_reads`, and `proc_macro_expansion` with `MacroCallId::expansion_reads`. `deps/key.rs`
  and `deps/reads.rs` compile once they land.
- usercode (`v2/requests/usercode.md`): applied (`Kept::code`, `report_reads`,
  `index::find_reads`, `blaze_expand::reads_now`). It builds once db-expand's half lands.
  `ExpressionStoreRef::decode_in` (mem-format-hir-apply) and `Frozen.bytes: PartBytes`
  (db-expand-names) have landed, and neither is built yet.
- cargo (`v2/requests/cargo.md`, amended): the adapter over `Reader::pinned`, `reads_now`, and
  `ReadThrough` once the global adapter exists.
- fastcompile (`v2/requests/fastcompile.md`): a public proof for dependency-image keys and an
  `ImageStore` over the global client.
