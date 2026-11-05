# rust-lang/rust-analyzer#23181: internal: Break hir-ty up, step 1

Author: ChayimFriedman2. Merged: 2026-10-12T17:08:53Z. https://github.com/rust-lang/rust-analyzer/pull/23181

According to the plan outlined [in Zulip](https://rust-lang.zulipchat.com/#narrow/channel/185405-t-compiler.2Frust-analyzer/topic/Breaking.20up.20hir-ty/near/617085080).

This first step extracts the `hir-ide` crate, containing extra diagnostics, display, and tests. Tests must be in the leaf crate because they need access to the full database (some can go with a dummy impl for some methods, but it'll be more complicated than just moving them here).

We had to make some compromises; pretty-printing MIR and MIR errors requires display, so we need it to be in `hir-ide`, but then it can't be an inherent method, so we made it an extension trait and also made a proc macro to make that easier (copied from rustc).

`hir_ty` was find-and-replace'd with `hir_ide` in `crates/hir/`.
