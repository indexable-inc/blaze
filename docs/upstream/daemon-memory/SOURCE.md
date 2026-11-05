# docs/upstream/daemon-memory: what was read, from where, when

Read on **2026-10-21** for [docs/research/daemon-memory.md](../../research/daemon-memory.md).
Excerpts of salsa 0.28.5, the version blaze's `Cargo.toml` pins, as cargo unpacked it from
crates.io (`~/.cargo/registry/src/index.crates.io-*/salsa-0.28.5`). Licence: MIT OR
Apache-2.0. Verbatim; each file name says the upstream file and its lines, with `/` as `_` and
`.txt` added so no Rust tool reads them.

| file | what it shows |
|---|---|
| `salsa-0.28.5_src_function.rs-505-515.rs.txt` | `reset_for_new_revision`: an `lru` evicts only when a new revision begins |
| `salsa-0.28.5_src_function_memo.rs-87-101.rs.txt` | `evict_value_from_memo_for`: the value goes, the memo's revisions and dependencies stay |
| `salsa-0.28.5_src_function_execute.rs-118-165.rs.txt` | `execute_maybe_iterate`: a query with cycle handling asks `previous_iteration` of an old memo verified in this revision |
| `salsa-0.28.5_src_function_execute.rs-364-396.rs.txt` | `previous_iteration`: such a memo without a value is taken for a cycle head that panicked, and `Cancelled::PropagatedPanic` is thrown |
| `salsa-0.28.5_src_function_execute.rs-510-562.rs.txt` | `PoisonProvisionalIfPanicking`: the memo a panic leaves has a `fixpoint_initial` origin, which an evicted memo does not |

Written here on **2026-10-22** for [#57](https://github.com/indexable-inc/blaze/issues/57), not
fetched:

| file | what it is |
|---|---|
| `salsa-0.28.5-evicted-memo.patch.txt` | the patch to salsa 0.28.5 that tells an evicted memo from one a panic left, and evicts no provisional memo; paths are salsa's own. Applied to `vendor/salsa` on 2026-10-26 for [#261](https://github.com/indexable-inc/blaze/issues/261) |
| `salsa-issue-lru-cycle.md` | a draft of the salsa issue that proposes it, not filed |

The same `previous_iteration` is in salsa's latest source on docs.rs
(`https://docs.rs/salsa/latest/src/salsa/function/execute.rs.html`, read the same day).
