# astlog

Datalog over tree-sitter syntax trees. Query matches become relations, rules join them by tree position, by value or recursively, and derived rows become lint findings or rewrite edits. "An `unwrap()` inside a function returning `Result`" is one rule here. In ast-grep or Semgrep it is a join those tools cannot express.

`astlog-engine` (`engine/`) is the language and its evaluator, generic over a fact source. `astlog-core` (`core/`) is the tree-sitter source: the `match` atom, the corpus, edits. blaze's own front end, Rust patterns matched by name resolution and types, is `crates/blaze-astlog` over the same engine (`docs/v2/plan/design/astlog-sema.md`); the daemon links the engine and never tree-sitter. blaze's rules live in `rules/`, with a good/bad fixture pair per rule under `rules/tests/`, and `scan.py` runs them over the tree (see [Rules in blaze](#rules-in-blaze)). Copied from ix; `UPSTREAM` names the commit.

## The language

A rules file is S-expressions in four forms:

```lisp
;; one row per match, one column per @capture
(rule (unwrap-call call e)
  (match rust "
    (call_expression
      function: (field_expression value: (_) @e field: (field_identifier) @m)
      arguments: (arguments)) @call")
  (text m "unwrap"))

(rule (result-fn f)
  (match rust "
    (function_item return_type: (generic_type type: (type_identifier) @r)) @f")
  (text r "Result"))

;; shared variables join, like shared columns in SQL
(rule (fixable call e)
  (unwrap-call call e)
  (result-fn f)
  (ancestor f call))

;; a derived row becomes an edit
(rewrite unwrap-to-try (fixable call e)
  (replace call "{e}?"))

;; a derived row becomes a finding at its first node-valued column
(lint fixable error "unwrap inside a Result-returning function: `{e}`")
```

Rules may be recursive. Every value is a node, text from a rules file or a node, or an integer from a literal or a `count`, so the universe is finite and the fixpoint terminates.

Negation and counting are stratified:

```lisp
;; holds when no row of `called` matches; every variable must be bound to its left
(rule (dead f) (fn-item f) (not (called f)))

;; binds n to the number of `(caller g f)` rows for the f bound on the left; g is local
(rule (hot f) (fn-item f) (count n (caller g f)) (lt 9 n))
```

A program whose negation or count sits on a recursive cycle is a load error naming the relations on the cycle. A variable used inside `(count ...)` and not bound before it is local to the count and may not appear outside it.

Rewrite actions are `(replace v "t")`, `(delete v)`, `(insert-before v "t")` and `(insert-after v "t")`. A source may add its own actions. Templates splice variables as the source writes them: `{e}` here, `$e` (or `${e}`, with `$$` for a literal `$`) in blaze's front end, whose templates are Rust where `{` is common.

Builtins:

- `ancestor`, `parent`. The lower argument must be bound by an atom to its left, and the error says so when it is not.
- `text`, `kind`, `same-text`, `same-file`, and `lt`, `le` over integers.
- `(file-path <node> <path>)` binds the path the file was loaded under, to scope a rule to `tests/`.
- `(text-match <node-or-text> "<regex>")`. The pattern must be a string literal.
- `(misgrouped-digits <node-or-text> <fixed>)` holds when a numeric literal's underscore grouping is not canonical threes, and binds the regrouped text, for a digit-grouping lint and its fix.
- `(no-descendant <node> "<kind>" "<text>")` holds when no strict descendant has that kind and exact text, and `(attached-sibling <node> "<kind>" "<text>")` when an attribute-like sibling directly above the node has such a descendant. Both are tree-sitter atoms, not engine builtins.

The engine checks modes at load: an argument a builtin or source atom needs bound must be bound by an atom to its left, and a head or template variable must be bound by the body. A quantified capture (`+`, `*`) contributes its first node per match. An optional capture (`?`) absent from a match is an error naming the capture, the rules line and the file (`CaptureAbsent`), never a silently unbound column; split the pattern instead. Patterns are verbatim tree-sitter queries, so a misspelled node kind or field is a load error with a position, never an empty result. `#eq?` and other `#` predicates are refused; use the builtins. Languages are every grammar `ast-merge-langs` registers, named by profile name or extension.

## CLI

- `astlog query rules.astlog src/ [--relation r] [--json]` prints derived relations.
- `astlog scan rules.astlog [paths...] [--json] [--error]` is the lint gate. It sorts findings by file, line, column and rule, and exits nonzero when an error-severity finding survives suppression. `--error` promotes warnings for the exit code. `--json` emits `{"rule","severity","message","file","line","column","endLine","endColumn","text"}` objects, a contract downstream tools parse.
- `astlog suppressions rules.astlog [paths...] [--json]` lists every finding an `astlog-ignore` comment hid, with that comment. `--json` adds `commentLine` and `commentText`.
- `astlog fix rules.astlog src/ [--write]` applies rewrites.

## Suppression

A comment containing `astlog-ignore` suppresses findings on its own line or the line below. `astlog-ignore: name1, name2` limits it to those rules. A comment is any node whose kind contains "comment". Suppression filters at scan output only, so the rows still exist for joins and `query`.

## Rules in blaze

`rules/` holds blaze's rules files, run by blaze.yaml's workspace check after `blaze check --workspace`:

- `rust-locks.astlog` (from ix): the same lock taken twice in one statement while the first temporary guard still lives, which deadlocks.

Lints that need types (the locks, channels and hashers ports: `blaze::std_lock`, `blaze::std_mpsc`, `blaze::fx_hash`) are blaze-lints', with machine-applicable fixes that `blaze check --fix` applies, not astlog rules: syntax alone cannot tell a parking_lot lock from another.

`scan.py` is the gate: every rules file over every file git has under the paths it is given, one file per `astlog scan`, since a join over one file stays small where one over the whole tree would not:

```sh
blaze build -p astlog
PYTHONPATH=tools/typed-input/src python3 tools/astlog/scan.py \
    '{"astlog": "target/debug/astlog", "rules": "tools/astlog/rules", "paths": ["crates", "tools"]}'
```

With `"fix": true` it applies the rules' rewrites instead, file by file, and lists the files it changed.

A rule's fixtures are `rules/tests/<rules file>/`: `bad.fixture`, the findings it must report in `bad.expected` (`line:column rule`, in scan order), `good.fixture`, which is `bad.fixture` fixed (or the same rules' clean case when they have no rewrites), and any other `good*.fixture`, which must report nothing and need no fix. `core/tests/rules.rs` checks them, and a new rules file needs a test there.

## Prior art

| Project | Has | Lacks here |
| --- | --- | --- |
| [treeedb](https://github.com/langston-barrett/treeedb) | tree-sitter ASTs as Souffle relations | rewriting; unmaintained since about 2022 |
| [Logifix](https://github.com/lyxell/logifix) | Datalog-driven rewriting | Java only; rules compiled by Souffle, not loaded at runtime |
| [ql-grep](https://github.com/travitch/ql-grep) | CodeQL syntax over tree-sitter | rewriting |
| [CodeQL](https://codeql.github.com) | the relational model with real dataflow | embedding, rewriting; heavy per-language extraction |
| [ast-grep](https://ast-grep.github.io), [GritQL](https://github.com/biomejs/gritql) | patterns, `inside`/`has`/`where`, rewrites | value joins, recursion, cross-pattern joins |
| [Semgrep](https://semgrep.dev), [Coccinelle](https://coccinelle.gitlabpages.inria.fr/website/) | metavariable patterns, diff-style patches | a general relational layer |
| [weggli](https://github.com/weggli-rs/weggli) | C/C++ sketches over tree-sitter | other languages, rewriting |
| [Glean](https://glean.software) | code facts queried relationally at scale | embedding; it is an indexing service |

The evaluator is semi-naive per stratum: each rule is compiled to a join plan whose slots are constants, bound inputs, outputs or checks; relations keep a hash index for every bound-column set a plan probes; and each round joins one atom against the previous round's new rows and the rest against everything known. Source atoms are demand-driven, asked only with the arguments their left context binds. The first naive evaluator stays in `engine/src/naive.rs` as the reference model for a Hegel property test. It was written here because rules load from a file at runtime. `ascent` and `crepe` fix rules at compile time, `datafrog` needs static tuple types and hand-written join plans, Souffle is an external C++ codegen step, and `cozo` would impose CozoScript as the user language. The S-expression reader is local too, because per-form line numbers drive every diagnostic. If rule sets outgrow it, magic-set rewriting is the next step; the language would not change.
