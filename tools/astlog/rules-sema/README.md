# rules-sema

astlog rules over blaze's own front end: Rust code patterns matched by name resolution and types, joined, recursed and negated ([the language](../../../docs/v2/plan/design/astlog-sema.md)). Each file holds one concern and says what it finds, why it matters in this tree, and why it is a rule and not a type or a lint (rule 73). None runs in a gate yet.

## Tests

`cargo test -p blaze-astlog --lib rules_sema` runs every rule here over its fixture crate `tests/<rule>/` through a real database (`crates/blaze-astlog/src/tests/rules_sema.rs`). A lint's findings must equal `expected.txt`, and a rewrite's `src/lib.rs` must become `fixed.rs`. A rule file without a test, or a test without a fixture, fails `every_rule_of_the_pack_has_a_test_here`. The fixtures stand on a small stub `std` in that file, so a rule that needs more of `std` needs the stub to grow.

The fixture crates have their own `[workspace]` and are not members of blaze's. The allocs check reads `crates/` only.

## Includes

`lib/calls.astlog` is the call graph and `lib/int-arms.astlog` the arms of hand-kept integer tables. A rule includes one by its path from the checkout root:

```
(include "tools/astlog/rules-sema/lib/calls.astlog")
```

The daemon resolves an include from the root of the view it serves, never from the including file or the disk. In a blaze checkout that path exists. Outside one, copy `lib/` to `tools/astlog/rules-sema/lib/` in your checkout, inside the daemon's view of it, or the load fails naming the path and the including line.

## Running one on a checkout

From the checkout's root, with the rule file anywhere:

```sh
blaze check --query path/to/tag-collision.astlog          # prints findings; exits 101 on an error lint
blaze check --rules path/to/cow-to-string.astlog --dry-run   # prints the diff, writes nothing
```

To run a fixture by hand, copy it out of blaze first: inside blaze's checkout the daemon loads the whole tree, and a query on a debug blaze did not finish in 300 s.

```sh
cp -R tools/astlog/rules-sema/tests/tag-collision /tmp/fx
mkdir -p /tmp/fx/tools/astlog/rules-sema && cp -R tools/astlog/rules-sema/lib /tmp/fx/tools/astlog/rules-sema/
cd /tmp/fx && git init -q && git add -A && blaze check --query ~/blaze/tools/astlog/rules-sema/tag-collision.astlog
```

## The rules

- `variant-table-disagrees`: a variant whose integer differs between a `k => E::V` table and an `E::V => k` table of one file.
- `tag-collision`: two variants given one integer in one `E::V => k` table.
- `int-arm-variant-picker`: a `match` that picks variants of one enum by four or more hand-kept integers.
- `picker-count-const`: a `match` over `x.below(N)` or `x % N` whose arm count is not the const `N`.
- `registry-misses-const`: a `const X: T` in no `[&T; N]` registry array of its module.
- `cargo-home-homes`: each of two or more functions that derive cargo's home themselves.
- `current-exe-respawn`: `current_exe()` read where the function, or one it calls, runs a program.
- `stdout-in-spawned-thread`: a `thread::spawn` closure that reaches `io::stdout()`.
- `frame-current-outside-task`: an uncalled function that reaches `frame::current` in no `frame::task`.
- `enum-cast-index`: `a[e as usize]` where `e` is an enum.
- `cow-to-string` (rewrite): `x.to_string()` on an owned `Cow<str>` becomes `x.into_owned()`.
- `deprecated-forwarder-port` (rewrite): each call of a `#[deprecated]` fn whose body only forwards its one or two parameters, in order, names the replacement.
