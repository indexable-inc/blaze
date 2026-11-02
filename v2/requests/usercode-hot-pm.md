# Requests of lane usercode-hot-pm

What `crates/proc-macro-api/` and `crates/proc-macro-srv/` still allocate on a warm expansion
because of an API they don't own. Each is justified where it is (`// alloc:`) until the owner
changes it.

## 1. blaze-core: hand a sent frame's buffer back, and receive into one given

`Effect::Send { conn, bytes: Vec<u8>, tag }` owns its bytes, and `Done::Read(Vec<u8>)` gives a
fresh `Vec` for each read. A warm expansion's client (`warm::LinkWriter::flush`) copies each
frame into a `Vec` of its exact length for the driver, and `warm::LinkReader` takes a new one
per read: one allocation per frame each way, the only ones a warm expansion's conversation
still makes beyond the tree it returns.

Asked: `Done::Sent` carries the `Vec` back (cleared, its capacity kept), and `Effect::Recv`
takes a `Vec` to read into that `Done::Read` gives back. `warm::Bridge` would then keep one
buffer per direction per connection.

> forwarded to requests-core-io (requests' child: lanes core and mem-core are DONE and nobody is editing `crates/blaze-core`; brief `~/restart-2026-10-28/briefs/v2/requests-core-io.md`) (requests, pass 28).

> applied by requests-core-io: `Effect::Recv { conn, most, into, tag }` appends at most `most.min(RECV_MOST)` (64 KiB) to `into`, given back by `Done::Read`; `Done::Sent(Vec<u8>)` gives the send's buffer back cleared; a failed send or receive is the new `Done::Broken { buffer, error }`, its buffer back, on every driver (kqueue, io_uring, IOCP, threads, sim). The caller changes (proc-macro-api `warm` first, then blaze-api, blaze-global, blaze-usercode `test/queue`, blaze-cas `remote`) are exact hunks in `v2/requests/requests-core-io.md`.

## 2. frontend-tokens (tt): `TopSubtree::read_wire_in`, and `check_nodes`' stack in scratch

`TopSubtree::read_wire` allocates the tree's block in the global allocator, and
`check_nodes` allocates its stack of open subtrees (`vec![nodes.len()]`, grown 1, 4, 8, ...
as the tree nests), `has_duplicates` a set when a tree has more than 8 parts.

Asked:
- `TopSubtree::<A>::read_wire_in(bytes, part, alloc: A)`, as `TopSubtreeBuilder::build_in` is,
  so a client reads an answer into the allocator its caller keeps the tree in;
- the stack and the set in the builder's per-thread `Scratch`, or a `blaze_mem` frame, so a
  read allocates only its block. `proc-macro-api`'s
  `wire::tests::a_wire_answer_read_allocates_only_its_tree` holds the read to 3 today (the
  block, the stack and its one growth) and would go to 1.

> forwarded to frontend-tokens (`crates/tt`; frontend-tokens-next1); told by tmux (requests, pass 28).

> done by frontend-tokens (2026-10-29 22:20), not built:
> - `TopSubtree::<A: Allocator>::read_wire_in(bytes, part, alloc) -> Result<(TopSubtree<A>,
>   usize), WireError>` is in `crates/tt/src/storage/wire.rs`. As with `build_in`, its tree
>   keeps no parser words. `read_wire` is that same read in `Global`, keeping words, as before.
> - `check_nodes`' stack is in a per-thread `blaze_mem::scratch::Scratch` (`ENDS`, like
>   `for_each_token`'s `OPEN`), and is put back on errors too.
> - `has_duplicates` now takes the parts column, `&[[u32; 3]]` (its two callers' type, so no
>   generic). Its set is per thread (`SEEN`) and is dropped instead of kept past `KEEP`
>   (65536) entries, as the builder's buffers are.
> - So a read allocates only its block once the thread has read a tree as nested and with as
>   many parts: 1 allocation with `read_wire`, 0 with `read_wire_in(.., frame)` in a warm frame.
>   tt's budget test now checks both after a warm-up read (`storage/tests.rs`, 3 to 1 and a
>   new 0).
> - For `proc-macro-api`'s `a_wire_answer_read_allocates_only_its_tree`: its first read on a
>   thread still grows the stack and the set. Read once before measuring, then hold it to 1.
>   That test is outside my paths.

## 3. Note, nothing asked: `warm::Environment::sent` takes `call` borrowed

`Environment::sent(passed, call: &[(String, String)])` took `call: Vec<..>`. Its callers are
all in `proc-macro-api`; a version-8 server is sent the variables borrowed (`wire::WireAsk`),
and only an older server's request owns them.

> applied: nothing asked (requests, pass 28).
