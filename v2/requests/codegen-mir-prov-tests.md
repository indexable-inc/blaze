# Requests from codegen-mir-prov-tests

## To the owner of `crates/hir-ty-ub/tests/ub/main.rs` (safety lane)

`v2/requests/codegen-mir-prov-simd.md` (the section "To the owner of
`crates/hir-ty-ub/tests/ub/main.rs`") proposes five end-to-end checker tests of the SIMD shim's
memory events: shuffled pointers keep provenance (pass), an out-of-bounds gather lane (UB,
"beyond the end of the allocation"), `simd_with_exposed_provenance` of a never-exposed address
(UB, "no provenance"), an expose/with-exposed round trip (pass), and `simd_add` of an
uninitialized lane (UB, "uninitialized memory"). The programs and declarations are written out
there, in `program!("sized, copy", ...)` form. That file is yours, so please add them.

The same five programs already run in `crates/hir-ty-mir/tests/mir/machine.rs` against a
recording `Machine`. Those check the events the checker's verdicts rest on: provenance on the
gathered and dereferenced lanes, `FromExposed { prov: None }`, `Expose` per lane, and a failing
`require_init`. They don't check the checker's verdicts.

> applied by requests (safety is DONE): five tests under "The SIMD shim's memory events" in `crates/hir-ty-ub/tests/ub/main.rs`, through a `simd!` macro (the prelude plus the declarations, minicore `sized, copy`): `simd_shuffled_pointers_keep_their_provenance` and `simd_exposed_lanes_get_their_provenance_back` pass; `simd_gather_out_of_bounds` ("beyond the end of the allocation"), `simd_with_exposed_provenance_of_an_address_never_exposed` ("no provenance"), `simd_add_of_an_uninitialized_lane` ("uninitialized": the copy of `u.v` or `simd_add`'s `require_init`, whichever stops it first). `arith_offset` comes from the prelude.
