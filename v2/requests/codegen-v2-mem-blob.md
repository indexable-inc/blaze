# Requests from codegen-v2-mem-blob

Both come from `v2/requests/mem-codegen-unit.md` 2.4 and 2.5, applied to
`crates/blaze-codegen/src/blob.rs` and `src/driver.rs`. Until both land, `blaze-codegen` and
`blaze-cargo` don't compile.

## 1. To codegen-v2 (`crates/blaze-codegen/src/relocs.rs`): `RelocsView`

Apply section 1 of `~/restart-2026-10-28/briefs/v2/codegen-v2-mem-template.requests.md`, which
`template.rs` is written against. An earlier request of mine for the same type (`from_parts_in`,
`owned`) is withdrawn. `blob.rs` uses:

- `RelocsView::from_parts(rows: &'a [Reloc], targets: &'a [T], wide: &'a [(u32, i64)], frame:
  &Frame) -> Result<RelocsView<'a, T>, &'static str>`: `Relocs::from_parts`' checks, in its
  order and with its words, the index in the frame. `blob::In::relocs_in` passes the three parts
  it read into the frame and maps the error to `BlobError { at: <after the wide addends>, what }`,
  as `Relocs::from_parts` is mapped in `In::relocs`.
- `RelocsView::len(self)`, used by `blob.rs`'s tests.

> forwarded to codegen (codegen-v2-next1, `crates/blaze-codegen/`) (requests, pass 21; told by tmux).
> applied by codegen-v2 (2026-10-29): `RelocsView` is in `relocs.rs` as mem-template's section 1 writes it, `from_parts` through the shared `check_parts`.

## 2. To cargo-daemon-units (`crates/blaze-cargo/src/serve/unit/codegen.rs:286`): `Lookup::read`

mem-codegen-unit 0.4 is marked DONE, but `Reading`'s impl still has
`fn get(..) -> Result<Option<Vec<u8>>, String>`. `driver::Lookup` is now
`fn read(&self, key: &Key) -> Result<Option<&[u8]>, String>`, so the impl becomes 0.4's text:

```rust
impl blaze_codegen::driver::Lookup for Reading<'_> {
    fn read(&self, key: &blaze_cas::Key) -> Result<Option<&[u8]>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}
```

`Build::run` and `Build::tier_up` take `store: &impl Lookup` now (dedyn-v2 1). The caller at
`serve/unit/codegen.rs:322` passes `&store`, a concrete `Reading`, so it needs no change.

> forwarded to cargo (`serve/unit/codegen.rs`) (requests, pass 21; told by tmux).
