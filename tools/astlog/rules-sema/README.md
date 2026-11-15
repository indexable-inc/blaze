# rules-sema

astlog rules over blaze's own front end: Rust code patterns matched by name resolution and types,
joined, recursed and negated ([the language](../../../docs/v2/plan/design/astlog-sema.md)). Each
file holds one concern, says what it finds, why it matters in this tree, and why it is a rule and
not a type or a lint (rule 73). None runs in a gate yet.

## Running one

A fixture crate is `tests/<rule>/`: the shapes the rule must find, the look-alikes it must not,
and what `blaze check` gave. Copy it out of the repository first: run inside blaze's checkout,
the daemon loads the whole checkout into its view and a query on a debug blaze did not finish in
300 s.

```sh
cp -R tools/astlog/rules-sema/tests/tag-collision /tmp/fx && cd /tmp/fx && git init -q && git add -A
blaze check --query ~/blaze/tools/astlog/rules-sema/tag-collision.astlog   # compare expected.txt
blaze check --rules ~/blaze/tools/astlog/rules-sema/cow-to-string.astlog --dry-run   # expected.diff
```

`--query` exits 101 when an `error` lint finds something.

## The rules

Lints:

- `variant-table-disagrees`: a variant whose tag differs between a `k => E::V` table and an `E::V => k` table of one file (a codec's decoder and encoder).
- `tag-collision`: two variants given one tag in one `E::V => k` table.
- `int-arm-variant-picker`: a `match` that picks variants by four or more hand-kept integers.
- `picker-count-const`: a `match` over `x.below(N)` or `x % N` whose arm count is not the const `N`.
- `registry-misses-const`: a `const X: T` in no `[&T; N]` registry array of its module.
- `cargo-home-homes`: each of two or more functions that derive cargo's home themselves.
- `current-exe-respawn`: `current_exe()` read where the function, or one it calls, runs a program.
- `stdout-in-spawned-thread`: a `thread::spawn` closure that reaches `io::stdout()`.
- `frame-current-outside-task`: an uncalled function that reaches `frame::current` in no `frame::task`.
- `enum-cast-index`: `a[e as usize]` where `e` is no number, `bool` or `char`.

Rewrites:

- `cow-to-string`: `x.to_string()` on an owned `Cow<str>` as `x.into_owned()`.
- `deprecated-forwarder-port`: each call of a `#[deprecated]` fn whose body only forwards its one or two parameters, in order, names the replacement.
