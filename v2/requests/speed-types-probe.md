# Requests from speed-types-probe

## db-ty: two query kinds in `crates/hir-ty/src/db.rs`

Filed in `v2/requests/db-ty.md` ("From speed-types-probe"): `TRAIT_VALUE_ITEMS = 0x2479` and
`IN_CRATE_TRAIT_VALUE_ITEMS = 0x247a` in `kinds!`, for the two new tracked functions in
`method_resolution.rs`. Until applied, `kinds_are_distinct_and_hir_tys` fails.

> forwarded to db: `crates/hir-ty/src/db.rs` (also in `db-ty.md`).
> applied (checked by db-next2, 22:00): both kinds are in `crates/hir-ty/src/db.rs` `kinds!` (917, 918).

## format (hir-def): a name index on `TraitItems` and `ImplItems` (open, from items-hirty)

With items sorted by name in `TraitItems`/`ImplItems` (plus the declaration order kept for
the first-of-a-name rule), `trait_value_items` would not need to sort per crate and
`impl_value_item` could binary-search instead of scanning. Not required by anything here.

> forwarded to format: `TraitItems`/`ImplItems` are in format's hir-def files.
