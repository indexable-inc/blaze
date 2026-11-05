# rust-clippy README.md (master, fetched 2026-10-24), extract

A collection of lints to catch common mistakes and improve your Rust code.

There are over 800 lints included in this crate!

Lints are divided into categories, each with a default lint level.

| Category | Description | Default level |
|-----------------------|-------------------------------------------------------------------------------------|---------------|
| `clippy::all` | all lints that are on by default (correctness, suspicious, style, complexity, perf) | **warn/deny** |
| `clippy::correctness` | code that is outright wrong or useless | **deny** |
| `clippy::suspicious` | code that is most likely wrong or useless | **warn** |
| `clippy::style` | code that should be written in a more idiomatic way | **warn** |
| `clippy::complexity` | code that does something simple but in a complex way | **warn** |
| `clippy::perf` | code that can be written to run faster | **warn** |
| `clippy::pedantic` | lints which are rather strict or have occasional false positives | allow |
| `clippy::restriction` | lints which prevent the use of language and library features | allow |
| `clippy::nursery` | new lints that are still under development | allow |
| `clippy::cargo` | lints for the cargo manifest | allow |

Clippy can automatically apply some lint suggestions, just like the compiler. Note that `--fix`
implies `--all-targets`, so it can fix as much code as it can.

    cargo clippy --fix

You can add options to your code to `allow`/`warn`/`deny` Clippy lints:

* the whole set of `Warn` lints using the `clippy` lint group (`#![deny(clippy::all)]`).
* only some lints (`#![deny(clippy::single_match, clippy::box_vec)]`, etc.)
* `allow`/`warn`/`deny` can be limited to a single function or module using `#[allow(...)]`, etc.

If you do not want to include your lint levels in your code, you can globally enable/disable
lints by passing extra flags to Clippy during the run:

    cargo clippy -- -A clippy::lint_name
