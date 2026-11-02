# Requests of lane selfbuild

## 1. cargo + codegen: `[package.metadata.blaze] proc-macro-libraries` (a blaze extension)

What: a package's manifest may list proc-macro dependencies by package name:

```toml
[package.metadata.blaze]
proc-macro-libraries = ["proc-macro-test-impl"]
```

For each name, blaze links that dependency as a native shared library with blaze's list of its
macros (`BLAZE_PROC_MACROS`, as `crates/hir-ty-codegen/v2/roots/proc_macros.rs` writes it, the
library blaze's proc-macro server loads), whatever `native-proc-macros`/`BLAZE_PROC_MACRO_WASM`
say, and every crate of the listing package that has it as a dependency (normal or dev) is
compiled with `BLAZE_PROC_MACRO_LIBRARY_<NAME>` set to that library's path, `<NAME>` as cargo's
`envify` makes it (`crates/blaze-units/src/load/plan/env.rs`). The variable is part of the
compile's environment, so it keys the unit as `CARGO_BIN_EXE_<name>` does (the library's content
ID, not only its path, as `results.rs` does for programs). A test reading it may read the file
in the sandboxed-tests experiment.

Errors, each naming the manifest and the entry: a value that is not a list of strings; a name
that is not a dependency of the package; a dependency that is not a proc macro; a library that
can't be linked. No fallback (no wasm module in its place, no rustc).

Where: `crates/blaze-units/src/load/plan.rs` (the compile env beside `bin_exes`, and a
dependency edge on the link) and `plan/env.rs` (`CompileEnv`), the manifest reader for
`[package.metadata.blaze]`, the native proc-macro link (codegen); `docs/cargo.md` gets a section
once it works (it is a current doc, so not before).

Why: `crates/proc-macro-srv/proc-macro-test/build.rs` ran cargo (and so rustc) inside a build
script to make a proc-macro dylib for proc-macro-srv's dylib tests, which broke no-rustc and stopped
blaze loading its own workspace. That build script is gone: `crates/proc-macro-srv/proc-macro-test`
is now the proc-macro crate `proc-macro-test-impl`, a path dev-dependency of proc-macro-srv,
whose tests read `env!("BLAZE_PROC_MACRO_LIBRARY_PROC_MACRO_TEST_IMPL")`
(`proc_macro_test_dylib_path` in `crates/proc-macro-srv/src/lib.rs`). Until this lands those
tests don't compile under `blaze test -p proc-macro-srv`; `blaze check --workspace` (lib
targets) is unaffected. Change 20 (macros on blaze's own compiler) would make the server and
these tests obsolete, but it hasn't landed and the warm server is what runs native macros
today (`docs/v2/plan/lanes/usercode/macros.md`).

`given_by_cargo` (`crates/blaze-cargo/src/cargo/directives.rs`) refuses a build script setting
`BLAZE_PROC_MACRO_LIBRARY_*` (selfbuild-env).

> forwarded to cargo-next2 (`blaze-units` `plan.rs`/`plan/env.rs` and the `[package.metadata.blaze]` reader are cargo's) and codegen-next2 (the native proc-macro link); neither is DONE; both told by tmux (requests, pass 47). `docs/cargo.md`'s section is the coordinator's, once it works.
