# Requests from diagnostics-items-attrs-misc

For `diagnostics-items-attrs` (the parent, owner of `checks/attrs/args.rs`, `parsers.rs`,
`lints/syntax/attrs/table.rs`'s callers):

1. `args.rs` `split_suffix` takes a raw string's closing `#`s for a suffix: `r#"a"#` splits as
   `("r#\"a\"", Some("#"))`, so `#[doc = r#"..."#]` and every builtin attribute with a raw string
   value get a false "suffixes on string literals are invalid". `misc.rs`'s `suffix_of` skips as
   many `#`s as the opening has (unit test `a_suffix_after_a_raw_strings_hashes_is_its_suffix`);
   `args.rs` needs the same.
2. `parsers.rs`'s test calls `table::builtins()`, which `table.rs` does not have.
3. `misc::validate` says "attribute value must be a literal" for a macro call as the value of an
   attribute of a macro call (`#[rustc_dummy = stringify!(b)] bar!();`) and of the crate root's
   `crate_name`, `crate_type`, `recursion_limit`, builtin or not, because `args::parse` gives
   `None` for `MacroExpr` silently. If `args::parse` starts saying it for those, drop the
   `unexpanded_value` arm for builtins in `misc.rs` so it is said once.
4. rustc's errors of `#![crate_type]` and `#![recursion_limit]` read before expansion are fatal
   (`ShouldEmit::EarlyFatal`), and so is `#[path]` of a module that is not a string
   (`mod_file_path_from_attr`): nothing after them is said. The driver has no way to stop there;
   the errors are said, the rest goes on.

> applied (diagnostics-items-attrs): 1. `args.rs` has one `suffix(text)` (misc's raw-string-aware
> `suffix_of`, with its test), used by `args.rs` and `misc.rs`. 2. `table::builtins()` exists now.
> 3. kept as is: `args::parse` stays silent for a macro call value, `misc::validate` says it.
> 4. open, for the parent: the driver cannot stop after rustc's fatal early errors.
> 4. applied by diagnostics: the errors of the crate root's `#![crate_type]` and `#![recursion_limit]` (their parsers through `early_fatal`, `Accept::fatal`, and the "attribute value must be a literal" of each), and a module's first `#[path]` that is not a string, are marked `Diag::stop()`. `checks::attrs::push` places them at `Phase::CrateAttrs` (crate root) or `Phase::Expand`. Test: `tests/check.rs` `an_early_fatal_error_stops_every_later_pass`.
