---
status: plan
summary: One portable identity for an item, its item key, and interned ids that cannot leave the snapshot that made them: every record and every table that outlives a snapshot names an item by key, through one pair of functions, and the six other portable forms are deleted.
date: 2026-11-15
---
# Item identity

## The problem

An item id (`FunctionId`, `ImplId`, `TraitId`, ...) is a `Gid`: an arena's index, that arena's
generation and a slot (`4.1:17914`). The slot is the order interning happened to reach the item
in, on whichever thread got there. It means nothing in another process, after an edit, or after
`Memo::next` remakes the arena. Two classes of bug follow, and both were met on 2026-11-15:

- An id kept past its arena. The body store's `traces` table is keyed by `DefWithBodyId` and
  outlives each `Memo::next`; on blaze's own workspace it handed out ids of dropped arenas
  ("arena 904.6 is dead: this snapshot holds arena 904.7"), 47 times in one run.
- An id written to a record. It cannot be, so each record family translates it, and each has
  its own portable form: session bits or a relocation index (`hir/persist.rs`), a location
  (`deps/signatures/table.rs`), a `PortableIds` index (`deps/signatures/ids.rs`,
  `check_cache/ids.rs`), a `DefMap` position (`nameres/frozen.rs`, `signatures/key.rs`), a
  caller-given 32 bytes (`check_cache/port.rs`), `BodyFacts::def_key` (`body_store/infer.rs`).
  Several of those forms move on an edit that does not touch the item (a `DefMap` position
  moves when a module is added).

## The design

1. **Identity is the item key.** `blaze_items::ItemKey` already names an item by what it is: its
   parent's key, its kind, its name, and where it is written (the file's key and its AST id,
   which is a hash of the name and parent plus an index among equals; a macro-made item adds
   the call's place). It is the same in every process. It becomes the one portable form.
2. **A rename is a new item.** The name is in the key. A renamed function's work is found again
   because work is keyed by content with the item's own name left out (a body's key writes its
   own item as "self"), not because anything recognises the rename. No rename detection.
3. **An interned id is local to its snapshot.** It stays what it is today, a small number that
   compares in one instruction, so the type checker keeps its speed. What changes is where it
   may go: it does not implement the traits a record or a carried table needs.
   - `blaze_id::Portable` is implemented by keys, never by a `Gid` or a type holding one.
     blaze-format's `Record`/`Persist` impls and the keys of tables that outlive `Memo::next`
     require it.
   - A table that outlives a snapshot is keyed by `ItemKey`.
4. **One pair of functions.** `blaze_id::key_of(db, id) -> ItemKey` (the existing batched
   `item_key`) and `blaze_id::resolve(db, krate, key) -> Result<Id, NoItem>` (a lookup in the
   crate's key index, which a given crate's pages hold). Every encoder and decoder of a record
   goes through them; the six other forms are deleted with their tables.
5. **Order.** Nothing is ordered by an id (their order follows thread timing). A sort for
   output or for a key is by item key.

## What changes, by crate

- New `crates/blaze-id`: `Portable`, `ItemKey` (moved from blaze-items), `NoItem`, the two
  functions' traits. Types only; it compiles first and alone.
- hir-def: the key computation moves below blaze-items (it needs only hir-def's facts);
  `signatures/key.rs` names a module by its key, not its `DefMap` position; `frozen.rs`,
  `signatures/table.rs`, `signatures/ids.rs`, `hir/persist.rs` encode an item as its key.
- hir-ty: `check_cache/ids.rs`, `check_cache/port.rs`, `next_solver/persist.rs`,
  `infer/persist.rs`, `lower/persist.rs`, `body_store` (`traces` keyed by key).
- blaze-memo: no id is `Portable`; `session_bits`/`from_session_bits` become private to the
  crate once no caller is left.
- Everything else (about 18,600 uses of the id types in 46 crates): unchanged, since the id
  types keep their names and methods. The sites that do change are the ones that read a
  `Gid`'s bits or sort by an id (about 420: hir-ty 217, hir-def 83, hir-ty-codegen 60,
  hir-ty-identity 21, hir-ty-pure 18, hir-ty-mir 14).

## How the change is made

By astlog (rule 13), not by hand and not by a compile-and-patch script:

1. `tools/astlog/rules/identity-census.astlog`, a query: every call of `gid`, `from_gid`,
   `session_bits`, `from_session_bits`, `gid_bits`, `word_of` whose receiver or result is an
   item id; every `sort*`/`binary_search*`/`Ord` use over a slice of an item id; every struct
   field of a type that outlives a snapshot whose key is an item id. Its rows are the work list.
2. One rewrite per shape the census finds, run with `--dry-run` first.
3. What no rule fits goes to lanes, by file.

## The check

- Surface records of `core`, `alloc` and `std` are written twice, in two processes with
  different interning order (thread count 1 and N): equal bytes.
- The simulation test of a table carried across `Memo::next` with an arena remade
  (`a_reader_of_a_carried_table_looks_up_only_live_ids`) passes with the table keyed by key
  and no `Live` proof needed.
- blaze checks blaze's own workspace with no engine panic.

## Not decided here

- Per-module name resolution and dropping syntax trees: separate designs.
- Whether a second `impl` of the same trait for the same type name should be told apart by
  more than its index among equals (today: index; adding an equal impl before it moves it).
