---
status: plan
summary: blaze v2: diagnostics.
date: 2026-10-29
---
# blaze v2: diagnostics

See [plan/process/building.md](../process/building.md), section 18.

## Done when

- every program rustc rejects, blaze rejects, and every one rustc accepts, blaze accepts, with
  the same error codes and lint names, the same warnings, and the same primary spans, checked
  against rustc's UI tests
- the text is blaze's own, rendered for speed and for LLM readers; it need not match rustc's
  wording or layout. cargo's `--message-format=json` schema stays, because tools parse it
- ide-diagnostics is no longer needed by any build
- no unfinished code in the lane's paths: `rg 'unimplemented!|todo!'` finds only text blaze
  writes for the user, which stays as rustc writes it (the `todo!()` of the suggestions for
  E0004, E0005 and E0046 in `lower/missing_match_arms.rs`, `lower/non_exhaustive_let.rs` and
  `lower/trait_impl_missing_assoc_items.rs`, the examples of `emit/codes/E0373.md`, `E0706.md`
  and `E0772.md`, and the test expectations of both); tests for all of it are written; its
  report says `STATUS: DONE`
