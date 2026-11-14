---
status: plan
summary: astlog over blaze's own syntax trees and semantic database: Rust code patterns with `$name` metavariables whose paths match by resolution, demand-driven semantic relations (resolves, type-of, implements, callee, references, enclosing items, in-unsafe, in-test), a semi-naive evaluator shared with the tree-sitter source, and rewrites that go out through blaze-intent. One versioned CBOR request for an edit and one for a read-only query; the CLI is a client of both.
date: 2026-11-13
---
# astlog over blaze's semantics

## What

A refactor written once, run by a person or an agent:

```lisp
;; every `.unwrap()` on a `core::result::Result` inside a fn returning a Result
(rule (fixable call recv)
  (expr call "$recv.unwrap()")
  (type-of recv t)
  (type-matches t "::core::result::Result<_, _>")
  (enclosing-fn call f)
  (fn-return f r)
  (type-matches r "::core::result::Result<_, _>"))

(rewrite unwrap-to-try (fixable call recv)
  (replace call "$recv?"))

(lint fixable warning "unwrap in a Result fn: `$recv`")
```

- **One language.** astlog's reader, program checker and evaluator move into `tools/astlog/engine`
  (crate `astlog-engine`), generic over a fact source. The tree-sitter source stays in
  `tools/astlog/core` (crate `astlog-core`, its tests and the `astlog` CLI unchanged). The blaze
  source is `crates/blaze-astlog`. Nothing the daemon links depends on tree-sitter.
- **Code patterns.** `(expr n "...")`, `(type n "...")`, `(pat n "...")`, `(stmt n "...")`,
  `(item n "...")` and `(path n "...")` parse the text as that Rust form with `$name` (one node)
  and `$$$name` (a run of sibling nodes, possibly empty) metavariables, and bind `n` to each node
  of the target files that matches tree against tree, ignoring whitespace and comments. Each
  metavariable is a rule variable. One used twice must bind equal code (token-equal). A `match`
  atom in a blaze program is a load error that names the pattern forms.
- **Paths match by resolution.** A multi-segment path in a pattern is a def path from a crate
  name (`core::mem::take`, `::std::sync::Mutex`) and matches a path in the code only when that
  path resolves to the same item: through an alias, a glob or a re-export, and never another
  crate's item of the same name. A single-segment name that resolves through the std or core
  prelude matches the same way; any other bare name matches by text (it is a local, a field or a
  method name, whose meaning `resolves` and `callee` give). A pattern path that names nothing is a
  load error.
- **Rewrites.** `(replace n "tmpl")`, `(delete n)`, `(insert-before n "tmpl")`,
  `(insert-after n "tmpl")`, `(rename n "new_name")` (n a def; every reference moves, through
  blaze-intent), `(retarget n "krate::path")` (n a path node; written as the file sees the new
  item, imported when needed) and `(add-use n "krate::path")` (n any node of the file). Templates
  splice `$var`; `$$` is a literal `$`. An absolute path in a template (`::core::mem::take`) is a
  def path, written as the destination module sees it (`Module::find_path`), with a `use` added
  when the spelling needs one. A rewrite may hold several actions.
- **Refusals, each naming what it names.** Two edits that overlap (both rules and the file and
  range). A match whose range lies in a macro expansion and maps back to no single input range
  (where, which rule), unless the rule excluded it with `(not (in-macro-expansion n))`. A
  rewritten file that no longer parses (the file, the first syntax error); the whole request
  fails, nothing is written.
- **Surfaces.** The API first: `Edit::Rules { rules }` rides the existing `Request::Edit`, so the
  answer is the usual `Edited` (VFS ops and seeds, the way `blaze_intent::apply` sends them).
  `Request::Query { base, rules }` is read-only and answers `Response::Rows` with each relation's
  rows and each lint finding as data (file, range, rule, severity, message, bound values). Schema
  57. The CLI beside `--replace`: `--rules FILE` applies a rules file, `--query FILE` prints the
  rows (JSON with `--message-format json`, per `structured-output.md`), and `--dry-run` with
  `--rules` prints the diff and writes nothing.

## The relations

Values are syntax nodes (a file tree or a macro expansion tree), defs, types, crates' names as
text, text and integers. "Needs" is the argument a relation is demand-driven on: a rule must bind
it from an atom to its left, and a rule that does not is a load error naming the atom and the
argument.

| Relation | Needs | Gives |
|---|---|---|
| `(expr n "pat")` and the other pattern forms | nothing | every matching node and its metavariables |
| `(syntax n "KIND")` | nothing | every node of that `SyntaxKind` |
| `(resolves n d)` | `n` | the def a path, name, method call or field names |
| `(def-path d p)` | `d` or `p` | the def path from the crate name; or the def at that path |
| `(type-of n t)` | `n` | the type of an expression or a pattern |
| `(type-matches t "pat")` | `t` | holds when the type matches; `$name` in the pattern binds a type argument |
| `(implements t "krate::Trait")` | `t` | holds when the type implements the trait |
| `(callee n d)` | `n` | the fn a call or method call runs, through trait dispatch where the front end knows the impl |
| `(enclosing-fn n d)`, `(enclosing-impl n d)`, `(enclosing-module n d)` | `n` | the innermost such item |
| `(crate-of n c)` | `n` | the crate's name, as text |
| `(fn-param d i t)`, `(fn-return d t)` | `d` | a fn's parameter types by index (self is 0 when present), its return type |
| `(def-kind d k)`, `(visibility d v)` | `d` | `fn`, `struct`, ...; `pub`, `pub(crate)`, `private`, ... |
| `(attribute d "name")` | `d` | holds when the item carries that attribute |
| `(defines n d)` | `n` or `d` | the def an item's syntax declares; or that item's syntax |
| `(references d n)` | `d` | every name, path and method call in the target files that resolves to `d` |
| `(in-macro-input n)`, `(in-macro-expansion n)`, `(in-unsafe n)`, `(in-test n)`, `(cfg-off n)` | `n` | filters |

Kept from astlog: `ancestor`, `parent`, `precedes`, `text`, `kind`, `text-match`, `same-text`,
`same-file`, `file-path`, `misgrouped-digits`. New in the engine for every source: `(count c (rel
args...))` binds `c` to the number of distinct rows of a lower-stratum relation that agree with the
bound arguments, and `(lt a b)`, `(le a b)` compare integers; integer literals are bare numbers.

## The evaluator

Semi-naive per stratum: each round evaluates each recursive rule once per body position that
names a relation of the stratum, that position reading only the facts new in the last round.
Relation atoms join through hash indexes on the columns bound at that position, built once per
(relation, bound columns) and kept up to date as rows arrive. Variables are slots, not names.
Source atoms are memoized per bound-argument tuple. Stratified negation and `count`, every
variable bound; a program with no stratification is a load error naming the relations on the
cycle. The old naive evaluator stays as the reference model for a property test only.

## Cost against the floor (rule 67)

| Operation | Floor | This design |
|---|---|---|
| Load a rules file | read it once | the same, plus one parse of each pattern |
| A code pattern | look at the nodes of the pattern's root kind in files holding its literal tokens | the same: memchr for the pattern's rarest literal token prefilters files, one preorder walk of each survivor |
| A pattern path | one name resolution per candidate path | the same, memoized per node |
| `references d` | resolve the names spelled like `d` | memchr for the name, resolve each name with that text |
| A semantic relation | one front-end query | the same, through `Semantics`' memo |
| Fixpoint | derive each fact once per supporting delta | semi-naive with hash joins: each derivation is found once per body position holding a new fact |
| Apply rewrites | splice each changed file once, reparse it | the same, plus blaze-intent's own reparse for semantic actions |

## Not built

- Magic sets. Demand comes from the rule binding the argument on its left; a relation is never
  enumerated over the whole crate graph.
- Dataflow (taint, reaching definitions). `callee` follows calls; values are not tracked.
- Patterns inside macro input token trees. A macro's expansion is searched; the tokens a macro
  takes are matched only as text.
- Edits inside a macro expansion that do not map to one input range are refused, not split.
- Rules across checkouts: the target is the daemon's view of one workspace.
