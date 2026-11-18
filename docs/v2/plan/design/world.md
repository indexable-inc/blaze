---
status: plan
summary: "The world interface, W0: a world is a value (a tree digest), and seven calls (open, patch, read, check, diff, commit, explain) on it, one request and one response a connection, in one versioned schema of deterministic CBOR, the same bytes over a Unix socket or HTTP POST. The schema lives in crates/blaze-world-api, a TypeScript client in tools/world/, golden vectors both test; W1-W3 and the helper fill the calls in behind it."
date: 2026-11-17
---
# The world interface (W0)

An agent edits, checks and commits a program through seven calls on worlds. W0 is the schema and
a client; nothing answers yet. Rust: `crates/blaze-world-api` (types, encode, decode, the JSON
rendering, a sans-IO `Session` for one connection). TypeScript: `tools/world/` (`schema.ts`,
`cbor.ts`, `client.ts`). Golden vectors: `tools/world/vectors.json`, tested by both.

## Decisions

- **W-a. A world is a value**: the digest of a tree. The server keeps no state for a caller
  (rule 70); a world id is all a caller holds. `patch` applies a list of edits to one base world,
  in order, all or none, and names the new world; a refused patch names the edit's index and why,
  and makes no world. `commit` is compare-and-set: the checkout must still be at `expect`, or the
  reply is `moved {now}` and nothing is written. A patch with no edits answers its base world.
- **W-b. The calls**: open, patch, read, check, diff, commit, explain. Edits: write_file,
  delete_file, rename_file, replace_range, replace_item. A finding carries a code (rule or error),
  a message, its spot (item, path, item-relative place plus line and column for printing), notes,
  and a fix as edits (`[]` is no fix).
- **W-c. The wire**: one versioned schema of deterministic CBOR; one request and one response a
  connection or a POST, the same bytes over either; `blaze world '<json>'` takes the JSON
  rendering for a shell. The version is in every message; an unknown version, key or variant is a
  typed refusal; every length is checked against its limit before anything is sized by it.
- **W-d. The helper**: `blaze world serve {socket, expires_s}` answers on a Unix socket and exits
  when `expires_s` pass with no request (rule 70). Its contract is below; it is not written.

## The calls

| call    | fields (key: name)                              | replies                      |
|---------|-------------------------------------------------|------------------------------|
| open    | 2 checkout                                      | world                        |
| patch   | 2 base, 3 edits                                 | world; refused edit          |
| read    | 2 world, 3 what (file, item, dir; dir "" root)  | text; listing                |
| check   | 2 world, 3 select (`[]` whole), 4 lints         | checked                      |
| diff    | 2 from, 3 to                                    | changes                      |
| commit  | 2 world, 3 onto, 4 expect                       | committed; moved             |
| explain | 2 world, 3 subject (finding, item)              | explained                    |

Every call may also get `refused`: `no_such_world`, `not_a_checkout`, `no_such_path`,
`no_such_item`, `no_such_finding`, `wire {at, what, field, said}`, `io {path, message}`,
`unsupported {call, why}` (a call the server does not do yet: the honest answer before W1-W3).
Edit refusals (`edit {index, why}`): no_such_file, exists, out_of_bounds, not_char_boundary,
expect_mismatch, no_such_item, overlap, conflict. `replace_range {path, start, end, text, expect}`
carries the replaced text as `expect`, so a stale range is refused (`expect_mismatch`) and not
applied; a digest would force BLAKE3 into every client for a check the text does as well. This is
decided for version 1; a digest form of `expect`, for large ranges, is a version-2 field.

## The encoding

- A message is one CBOR map: key 0 the version (1), key 1 the call (request) or reply (response)
  code, keys 2.. its fields in the table's order. Every record is a map with keys 0, 1, 2, ... A
  variant's key 0 is its tag. Keys are ascending, each present, none optional: one value has one
  spelling. A key out of order, repeated, missing or unknown is refused by those names.
- Values: unsigned integers up to 2^53 - 1 (so JSON carries them exactly); ids are byte strings of
  exactly 32; text is UTF-8; booleans are `f4`/`f5`. No negative integers, floats, tags, null,
  undefined or indefinite lengths. Every head is shortest-form.
- Paths are relative and `/`-separated with no empty, `.` or `..` part, no NUL or `\`; a checkout
  is absolute with no `..`. Nesting is at most 8 deep (the schema's deepest: a finding's note's
  spot's place in a checked reply), so a reader's stack is fixed.
- A message is self-delimiting: no framing. On a socket the reader scans heads until the item
  ends; bytes past it are `second_request`. A peer that closes early gets `short`.

Codes (index = code): calls open 0, patch 1, read 2, check 3, diff 4, commit 5, explain 6.
Replies world 0, refused 1, text 2, listing 3, checked 4, changes 5, committed 6, moved 7,
explained 8. Edits write_file 0, delete_file 1, rename_file 2, replace_range 3, replace_item 4.
What file 0, item 1, dir 2; target path 0, item 1; subject finding 0, item 1. Changes file_added 0,
file_deleted 1, file_changed 2, item_added 3, item_removed 4, item_changed 5. Refusals edit 0,
no_such_world 1, not_a_checkout 2, no_such_path 3, no_such_item 4, no_such_finding 5, wire 6, io 7,
unsupported 8. Wire codes cbor 0, not_shortest 1, indefinite 2, unknown_key 3, duplicate_key 4,
key_order 5, missing_key 6, unknown_variant 7, version 8, too_long 9, too_many 10, too_large 11,
too_deep 12, bad_path 13, range 14, second_request 15, short 16, trailing 17. Entry kinds file 0,
dir 1.

Record keys:

| record   | keys                                                                      |
|----------|---------------------------------------------------------------------------|
| edit     | 0 tag; write_file 1 path 2 text; delete_file 1 path; rename_file 1 from 2 to; replace_range 1 path 2 start 3 end 4 text 5 expect; replace_item 1 item 2 text |
| what, target, subject | 0 tag, 1 path or key or id                                   |
| checked  | 2 world, 3 findings, 4 certified, 5 entered, 6 ms                         |
| entered  | 0 names, 1 heads, 2 bodies, 3 lints (counts of each re-entered)           |
| finding  | 0 id, 1 code, 2 message, 3 at (spot), 4 notes, 5 fix (edits)              |
| spot     | 0 item, 1 path, 2 place                                                   |
| place    | 0 start, 1 end (bytes from the item's start), 2 line, 3 column (1-based)  |
| note     | 0 message, 1 at (spots)                                                   |
| entry    | 0 name, 1 kind                                                            |
| change   | 0 tag, 1 path; item changes 1 item, 2 path                                |
| text     | 2 path, 3 start, 4 text; listing 2 entries; committed 2 files_written; moved 2 now; explained 2 why, 3 reads |

## Limits

Checked before allocation, the same on both sides (`Limits::DEFAULT`, `DEFAULT_LIMITS`): a
message at most 64 MiB (`too_large`, also refused at a string head that claims more); a text at
most 16 MiB and a path, name or code at most 4096 bytes (`too_long`); at most 4096 edits (a patch
or a fix) and 65536 entries of any other list (`too_many`); 16 entries in a map; depth 8
(`too_deep`). A refusal says the limit in `said`.

## The JSON rendering

For a shell and for logs: the same fields by name. A request is `{"v": 1, "call": "<name>", ...}`;
a response `{"v": 1, "reply": "<name>", ...}`, with `checked` putting the answer's fields at the
top. A variant is `{"kind": "<name>", ...}`. Ids are 64 lowercase hex digits; codes are their
names. `blaze world '<json>'` parses this, sends the CBOR, prints the reply as this.

## Worked by hand

`request open` of `/w` is `a3 00 01 01 00 02 62 2f 77`: `a3` a map of three; `00 01` v = 1;
`01 00` call = open; `02 62 2f 77` checkout = the 2-byte text `/w`.

`refused: edit` is `a3 00 01 01 01 02 a3 00 00 01 03 02 04`: a map of three; v = 1; reply 1 =
refused; key 2, the refusal, a map of three: tag 0 = edit, `01 03` index 3, `02 04` why 4 =
expect_mismatch.

## Examples

**One body, checked, fixed, committed.** `base = open("/w")`. `next = transaction(base, tx =>
tx.replaceItem(f, "fn f() -> u8 { 1u16 }"))` is one patch. `next.check({select: [], lints: true})`
answers one finding `E0308` at f's body with a fix of one `replace_range`. `transaction(next, tx
=> tx.apply(finding.fix))` makes `fixed`; its check is clean and `certified`. `fixed.commit({onto:
"/w", expect: base})` writes one file and answers `committed {files_written: 1}`.

**Two agents, one base.** Both open `/w` and get A. One patches to B and commits B expecting A:
committed. The other patches to C and commits C expecting A: the checkout is at B, so `moved {now:
B}` and nothing is written; it diffs A to B, patches B again with its own edits, checks, commits
expecting B.

**A world not on disk.** `transaction(base, ...)` makes C, which no checkout holds.
`C.check(...)` answers from C's tree alone: what `check` reads is the world, never the disk. Before
W1 this is `refused unsupported {call: check, why: "not on disk until W1"}`.

## Who fills what in

- **W1, worlds**: the tree store and digests behind `open`, `patch`, `read`, `diff`; a patch's
  tree shares every unchanged file and item with its base. W1 also makes the id one type: the
  tree's digest (blaze-answer's `TreeDigest`) moves to the lowest ids crate and `WorldId` becomes
  that type; nothing moves before W1.
- **W2, check**: `check` and `explain` from the item layer on a world's tree, findings as records
  (rule 81), `entered` from the engine's counts, `certified` when every body has its proof.
- **W3, commit**: `commit`'s compare-and-set onto a checkout: read the disk's digest, compare,
  write the changed files, sync, all under one lock on the checkout; a DST proves it (rule 43).
- **The helper**: `blaze world serve {socket, expires_s}` binds a socket only its user can reach,
  one `Session` per connection, exits after `expires_s` idle. `blaze world '<json>'` and the
  HTTP route call the same handler.

## Out

No watch, no subscriptions, no streaming replies, no server state between calls, no sessions or
logins on the socket, no batch of calls in one message. A transaction is one patch, built on the
client.
