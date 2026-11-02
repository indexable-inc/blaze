# hot-sweep-cg-rest requests

- `crates/hir-ty-codegen/mod.rs:7299` (`entry.code.code()?` on an `Arc<Entry>`, owner
  hot-sweep-cg-mod / codegen): a store hit clones the entry's bytes, lines, unwind, slots and
  signature through `CachedCode::code(&self) -> Code` (`cached.rs:247`). A cache hit should
  allocate nothing: have the stitcher (`define_code` and the debug path) take a borrowed view of
  `CachedCode` instead of an owned `Code`; `cached.rs` will then add `CachedCode::view(&self)`.
- `crates/hir-ty-codegen/unwind.rs` `UnwindContext::add_function(.., &FunctionUnwind)`: called
  from `mod.rs:7357`, which keeps the value. Passing `FunctionUnwind` by value would remove the
  per-function `landing_pads.clone()` and `seh::Tables::add_function`'s `frame.clone()`.
