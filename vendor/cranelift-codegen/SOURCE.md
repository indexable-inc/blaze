# cranelift-codegen

- Upstream: https://github.com/bytecodealliance/wasmtime (`cranelift/codegen`), crates.io
  `cranelift-codegen` 0.137.1 (sha256 be81b89cb7b186a26cf2014c312aeb17737da988b4444d16a0cb3424d637d44e; its `src/` is 0.137.0's byte for byte).
- Licence: Apache-2.0 WITH LLVM-exception (LICENSE).
- Left out: Cargo.toml.orig, Cargo.lock.

The copy is the crate unchanged except for one visibility and one fix, the whole diff against it:

```diff
--- cranelift-codegen-0.137.1/src/machinst/buffer.rs
+++ vendor/cranelift-codegen/src/machinst/buffer.rs
@@ -432 +432 @@
-    pub(crate) unwind_info: SmallVec<[(CodeOffset, UnwindInst); 8]>,
+    pub unwind_info: SmallVec<[(CodeOffset, UnwindInst); 8]>,
--- cranelift-codegen-0.137.1/src/ir/dfg.rs
+++ vendor/cranelift-codegen/src/ir/dfg.rs
@@ -210,2 +210,5 @@
         self.jump_tables.clear();
+        // blaze: upstream leaves the last function's exception tables here, which a reused
+        // `Context` then hands the next function built in it (vendor/cranelift-codegen/SOURCE.md).
+        self.exception_tables.clear();
         self.mem_flags.clear();
```

`MachBufferInner::unwind_info` was `pub` through 0.136; 0.137 made it `pub(crate)` with no
accessor, and the processed `isa::unwind::systemv::UnwindInfo` and the gimli FDE it makes keep
their rows private. hir-ty-codegen builds each frame's `.eh_frame` rows from these instructions
(`SystemV::rows` in `crates/hir-ty-codegen/unwind.rs`), and blaze-stencil's x64 tests read them.

`DataFlowGraph::clear` resets every table but `exception_tables` (upstream `main` too, as of
2026-11-02). hir-ty-codegen keeps each thread's `Context`s through `Context::clear` and builds the
next function in one (`crates/hir-ty-codegen/spare.rs`), so that function's IR held the exception
tables of the last function compiled on the thread. They print nowhere, but the stencil's `Hash`
is the function's shape (`v2/shape.rs`), so one function got another shape by what its thread
compiled before, and the memo refused it: "another build of the memo landed shape .. under the
same stored key" (rule 18). `spare.rs`'s `a_kept_context_hashes_as_a_new_one` pins it.

Drop when upstream exposes unwind_info and clears exception_tables: then delete this directory and
its `[patch]` line.

Every crate that depends on `cranelift-codegen` (frontend, module, object, interpreter, reader) is
pinned in the root to this version, so `[patch]` covers each and the build holds one copy.
