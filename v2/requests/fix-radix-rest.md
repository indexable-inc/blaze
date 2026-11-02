# Requests of lane fix-radix-rest

## 1. `crates/blaze-format/src/tiers.rs`: use `blaze_ids::radix::sort` (for lane format)

Why: `docs/v2/plan/goals/principles.md` ("Sorting and ordering") wants one radix sort for integer
keys, and `crates/blaze-radix/src/lib.rs` (lane fix-radix) is it. `tiers.rs` has a private
`fn radix_sort(values: &mut Vec<u32>)` (an LSD sort on bytes, `sort_unstable` below 256 values)
that is being written now, so I left the file alone. Once it settles, the change is: delete
`radix_sort`, and call `radix::sort(&mut distinct)` at its two call sites (the `Shape` pass and
the dictionary build), with `use blaze_ids::radix;` and `blaze-ids.workspace = true` in
`crates/blaze-format/Cargo.toml` if it is missing. The output is the same: the values are `u32`
and the order is ascending, so ties are equal values.

> forwarded to format.

