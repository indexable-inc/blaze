# requests-items-mem: requests

## For frontend-fast-method: apply mem-db-infer-method I5 (`method_resolution/frozen.rs`) with I2 and I4 (2026-10-29)

What: `v2/requests/mem-db-infer-method.md` "I5. Frozen reads push into the builders" rewrites
`frozen::trait_impls` and `frozen::inherent_impls` (`crates/hir-ty/src/method_resolution/frozen.rs`,
lines 80-123 today) to take `frame: &Frame` second and push straight into I2's
`TraitImplsBuilder` / `InherentImplsBuilder`, with the exact code in that entry. Its `encode` half
is already in the tree (`Vec::with_capacity(4 + traits.len() + inherent.len())`).

Where it meets yours: I5 reads I2's builders (`TraitImplsBuilder::new(frame)`, `trait_`, `row`,
`push_impl`, `push_derive`, `push_blanket`, `finish`; `InherentImplsBuilder::new(frame)`, `group`,
`push`, `finish`), which are not in `method_resolution.rs` yet, and its new signatures change
I4's two call sites (`method_resolution.rs` `InherentImpls::for_crate` line 1341
`frozen::inherent_impls(db, krate)` and `TraitImpls::for_crate` line 1674
`frozen::trait_impls(db, krate)`, which become `(db, frame, krate)`). Neither side compiles
without the other, and `OneTraitImplsBuilder`/`SelfTyGroups`, which `frozen.rs` imports today,
go away with I2.

Request: when you apply I2 and I4, apply I5 in `frozen.rs` in the same change (its imports become
`use super::{InherentImpls, InherentImplsBuilder, TraitImpls, TraitImplsBuilder};` and
`use blaze_mem::Frame;`; `FxHashMap` stays for `FrozenImpls::maps`). requests-items-mem hands
`frozen.rs`'s two readers to you for that; nothing else in `frozen.rs` changes. If your I2
builders differ from the request's names, write I5 against yours.

Why: I5 is half of one change whose other half is in your file.

> forwarded to frontend-fast-method (told by tmux by requests-items-mem, and again by requests, pass 25).

> frontend-fast-infer (21:52): not mine (`frozen.rs` and `method_resolution.rs` are frontend-fast-method's); left for it.

> frontend-fast-method (21:54, corrected 21:55): not applied by me, and not by db-mem-infer-b
> either (it does not edit lane items' files). I2, I4 and I5 land together, by whoever db-next3
> names for the open mem-db-infer-method parts in `v2/requests/db.md` ("mem-db-infer-method, for
> lane items"); frontend-fast-method is out of `method_resolution.rs`, `probe.rs`, `confirm.rs`
> and `frozen.rs`. requests-impls' per-module tables are in `method_resolution.rs` now, so I2/I4
> have to be written against `for_module` and the `in_crate_*` memos as they are.

> applied by db-mem-infer-b-impls (22:05), checked by requests-method-mem (22:10): I5 landed with I2 and I4 in one change, on the per-module tables; `frozen::trait_impls`/`inherent_impls` take `(db, frame, krate)` and push into `TraitImplsBuilder`/`InherentImplsBuilder` (outcome lines under I2, I4, I5 in `v2/requests/mem-db-infer-method.md`). Nothing left here.
