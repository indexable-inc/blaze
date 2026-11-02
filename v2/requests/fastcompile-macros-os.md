# Requests of fastcompile-macros-os

- `v2/requests/fastcompile.md`, "fastcompile-macros-os 1": codegen-mir: `Foreign::call` may
  decline a call (`Claimed::Declined`), which the evaluator then runs with its own shims.
  Applied by codegen-mir.
- To the parent, fastcompile-macros: `run.rs`'s `Foreign::call` maps `Os::call`'s
  `Ok(Answer::Answered)` to `Ok(Claimed::Answered)` and `Ok(Answer::Evaluator)` to
  `Ok(Claimed::Declined)`. Applied by the parent.
