---
status: plan
summary: What a link after a one-function edit costs that grows with the program, where, and the design that makes it cost the edit. Built - a link taken as a patch (the ranges written and a Merkle digest of the file kept by the blocks written), an unsynced edit-path commit with a digest check after a crash, a named link from scratch when a kept image has no room, and the spans. A linked image's store key is the digest's root, so no step hashes the file. Not built - span tables kept per owner, a store that takes a patch, the dev run's whole-file copy, one link for two paths.
date: 2026-11-10
---
# Patch in place

Measured 2026-11-10 on `fn main() { println!("hi {}", 1); }` after a one-line body edit, link
reported `spliced`: `unit link` 1028 ms, of which `link inputs` 668 ms. The linker's stage was
already a splice. What costs the program is around it.

## What grows with the program

| Cost | Where |
|---|---|
| `link inputs`: every item of the build is resolved, its anchors asked, the positions of each body the code names fetched and its span table encoded again, std's included | `crates/hir-ty-codegen/v2/linked.rs:112` (`link_inputs`), `:253` (`tables`), `:342` (`named_anchors`, every relocation of every function), `:474` (`encode`) |
| every debug name cloned, twice | `linked.rs:115`, `crates/blaze-cargo/src/serve/unit/codegen.rs:2288` |
| every function hashed whole for its row, then sorted | `codegen.rs:2025` (`filled_id`), `:2078` |
| every name, debug name and span table hashed for `given_id` | `codegen.rs:2458` |
| the address map and the perf map written whole | `codegen.rs:1877` (`Image::functions`), `:1888` |
| the file made whole from the image's state | `crates/blaze-link/src/lib.rs:856` (`Image::bytes`), `file.rs:223` (`whole`) |
| a dev run's file copied out of its code memory and hashed whole | `crates/blaze-cargo/src/serve/unit/run/inproc.rs:608`, `:609` |
| the file hashed whole for its store key, put whole, and the first place of it written whole | `crates/blaze-cargo/src/serve/unit/compile.rs:2100`, `:2108`; `crates/blaze-cas/src/lib.rs` (`place_file`, `CLONE_FROM`) |
| the writer's tables rebuilt and compared in 4 KiB blocks | `crates/blaze-link/src/file.rs:401`, `:565` (`changed_blocks`) |

Not costs: the build id is a hash of the lineage and generation (`state.rs:1142`), never of
the file. The two `link:` lines (`out/hello`, `debug/hello`) are one link and two places of one
store key, each a clone.

## Design

1. The link keeps its state between builds: `Linked` in the daemon's kept images, by path,
   let go when any field of its `Output` or its layout profile differs (`kept_image`). Its
   columns (function rows, static rows, natives, names) are the delta's other side.
2. The delta is computed in the link half from the build's whole lists: rows by key, one
   merge pass (`function_rows`, `static_rows`; span `link delta`). A row is the function as
   filled, so a caller whose callee moved is in the delta. Making the rows without hashing
   needs the driver to say what it filled; that is the walk's lane.
3. The stage places the delta in the room the image kept (new code in the patch region, the
   slot's cell rewritten, old code left for frames still in it). A change the image has no
   room for is linked from scratch, named: `Full::NoRoom` on the link line and a note with
   the linker's refusal, as `Full::NoThunk` already was. Before this it was a build error
   that said `blaze stop`.
4. The commit is taken as a patch (`Image::patch`): the ranges a file commit writes and the
   file's identity before and after. Span `link patch` records `written` and `ranges`.
5. The file a client runs is never written in place. That rule stays (`tests/splice.rs`):
   Linux refuses to write a file being run, and macOS kills a program whose signed file
   changed under its vnode. The O(edit) file update is a clone of the last file, `pwrite` of
   the patch's ranges, and a rename. `Image::commit` does exactly that. A second path to
   the same file is a clone or a hard link of the renamed file (`Effect::Link`), never a
   rewrite.
6. The file's identity is a Merkle tree over its 4 KiB blocks (`blaze_link::digest`), the
   root bound to the length. A patch of `k` blocks rehashes `k` blocks and `k * log2(n)`
   nodes. It stays a sound identity because it is a collision-resistant hash of the whole
   content: equal roots mean equal lengths and equal blocks. An XOR of block hashes updates
   as cheaply and is not one (a set of blocks XORing to a chosen value is linear algebra).
   The tree is sparse, since an image is mostly room: a subtree of zeros keeps no node.
7. Durability: the output is derived data. A patched clone is not synced (a file written
   whole still is). After a power loss the rename may outlive writes that did not land, so
   the file can be torn. The daemon's link state is memory and dies with it, so the first
   link after a restart is from scratch and writes the file whole: `first link`. Whoever
   patches a file it did not just write (a restored link state, a store's derived copy)
   calls `Image::verify` first; a file that is not the last link's is refused
   (`Refusal::Torn`, naming the first block that differs), the kept state is let go and the
   link is from scratch, said as such.
8. Live: a dev executable's link is committed into the daemon's code memory
   (`run/inproc.rs` `Loaded::patch` -> `Image::commit_into`), which writes only the
   stage's ranges and publishes each; the running program takes the message and switches
   slots in one atomic store (`blaze-live` `slots`). No relink, no restart. The safety rules
   are the existing ones, stated where upheld: new code never goes where old code was
   (`stage.rs`, tested `an_incremental_link_puts_the_new_function_in_the_old_slot`), no
   thread is stopped and nothing a thread can be in is written over (`blaze-live/src/lib.rs`),
   a patch whose code the loader would slide, or whose signature or layout assumptions
   changed, is a restart with the reason (`codegen.rs` `changed_assumptions`).

## What an edit writes now

The linker hands over: the new function's bytes, its caller's bytes where its fill changed,
the 8-byte jump-table cell, the changed 4 KiB blocks of the writer's tables (symbol, name,
unwind header) and the changed header blocks, neighbours closer than 4 KiB merged. The
counter test holds that to the function plus 16 blocks, and to no more among 200 functions
than among 20.

The daemon still makes the whole file from that (`link copy` span) because the store takes
whole files. So the linker's part is O(edit) and the handoff is not.

## The store's key of a linked image

A linked image is filed under `compile::linked_key(root)`, the root of the linker's digest,
in a domain of its own: `unit outputs hash` no longer passes over it (131 ms measured), nor
does a dev run's commit (`run/inproc.rs` `file_key`; two hashes of the file per link before).
It is a sound content identity for the reason in 6: the root binds the length and every
block. For a dev run the invariant is that only `Image::commit_into` and
`Image::roll_back_into` write the image part of its code memory, so the memory's bytes are
the file the digest names. The first link hashes the file's data once (not its room); an
edit hashes the blocks it wrote.

## Not built, and why

- **Span tables kept per owner** (the 668 ms). The table of an anchor is a function of the
  owner's positions, which only the query engine can say are unchanged. The engine exposes
  no stamp of "no library input changed since view V" (`Storage::last_changed` is private
  and renumbered), and file naming is the job's, so no key outside the front end covers
  every input. The fix is in `linked.rs`: keep `(rows, encoded)` per owner beside the
  database (as the walk keeps describes), ask `positions` (a memo verify, one compare for a
  library body) and encode only where the rows differ; and count named anchors from the
  delta's functions. It needs a profile first: nothing says which of resolve, verify or
  encode the 668 ms is. That file is the walk lane's.
- **A store that takes a patch.** `Put` holds whole bytes keyed by BLAKE3 of the file.
  The op needed: `derive(key = Patch::digest, base = Patch::base, writes)`, which clones
  `placed/<base>`, writes the ranges, and seals by the Merkle root (verified by
  `Digest::of` when it must be). `compile.rs` then carries `Written::Patched`. It is durable,
  crash-recovering store code and needs its own DST; `blaze-cas` and `job.rs` are other lanes.
- **The dev run's copy**: `Loaded::patch` still copies the memory out for the store's put
  (the hash is gone). It goes with the store's patch op.
- **Two links per program**: `out/hello` and `debug/hello` are each a kept image and a
  link (`compile.rs`, one `codegen::link` per path of `artifact::made`), so `dev run commit`
  runs twice. One image placed at two paths needs the unit's file list and `job.rs`.
- **Rows, `given_id` and the maps from `built.delta`**: the interface is not in the tree yet.
- **Function rows, `given_id`, the two maps**: linear with small constants; named by spans
  (`link function rows`, `link delta`, `link tables`).

## Tests

`crates/blaze-link/src/tests/patch.rs`: the refinement `a_patched_file_behaves_as_a_fresh_link`
(Hegel: edits at the same size, smaller, larger, add, remove, callee set, frame, static)
against a fresh link by normal form, with controls `the_oracle_catches_a_stale_unwind_entry`,
`the_oracle_catches_a_slot_left_on_the_old_body`,
`the_oracle_catches_a_removed_function_still_exported`,
`the_oracle_catches_a_write_the_digest_missed`; the counter
`a_same_size_body_edit_writes_the_function_and_a_constant`; the crash invariant
`a_torn_patch_is_never_taken_for_the_links_file` with
`the_oracle_catches_a_check_of_the_length_alone`. `crates/blaze-link/src/digest.rs`:
`a_digest_kept_by_updates_is_the_files` with `the_oracle_catches_a_block_not_rehashed`.
