# Requests from mem-core-linux

Report: `/tmp/blaze-lanes/v2-mem-core-linux.md`. Each request is for a path outside this lane's.

## The hot lint cannot be narrowed to a crate (tools/v2/hot_lint.py)

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["blaze-core"]}'`
fails with `TypeError: typed_input: crates has a type it does not read: list[str]`. `typed_input`
reads no `list[str]` field, so `Input.crates` can never be given. blaze-core is also not in the
lint's `HOT` set. Fix: teach `typed_input` lists of strings (or make `crates` a tuple it reads),
and add `blaze-core` to `HOT`. Until then this lane ran the lint's own `lint_file` on its four
files: 0 findings.

> applied by the coordinator (checked: `tools/v2/hot_lint.py` takes `crates: tuple[str, ...]` and `HOT` names `blaze-core`) (requests, pass 19).

## docs/README.md is stale

`python3 tools/docs.py check` says `docs/README.md is stale`. That covers this lane's new
`docs/v2/plan/lanes/mem/core/linux.md`, among others. Fix: `python3 tools/docs.py write` when the
lanes' docs land.

> forwarded to the coordinator (`python3 tools/docs.py write` once the lanes' docs land), on the requests report's list (requests, pass 19).

## `Slab` matches an even generation (blaze-mem)

The same as mem-core-macos's request in `v2/requests/mem-core.md`. `uring.rs`'s `conn_of` maps a
caller's even generation to a handle past every slot until it is fixed. Every other handle the
driver looks up is one it issued.

> forwarded to mem (mem-next1, `crates/blaze-mem/src/handle.rs`; the same as mem-core-macos's in mem-core.md) (requests, pass 19; told by tmux).

> done by mem (2026-10-29): `Slab::get`, `get_mut`, `remove` and `contains` now match only an
> occupied (odd) generation (`Entry::holds`, `crates/blaze-mem/src/handle.rs`). Test
> `slab_refuses_a_made_up_even_generation`: the removed slot's even generation and generation 0
> find nothing. A made-up even generation now simply misses, so the drivers' maps need no workaround.
