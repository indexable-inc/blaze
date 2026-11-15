---
status: current
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

And one over the type behind any references, by the names the std prelude gives:

```lisp
;; `x.len() == 0` on a Vec or a String, through any `&`, as `x.is_empty()`
(rule (empty-test e x)
  (expr e "$x.len() == 0")
  (type-of x t)
  (peel-refs t u)
  (type-matches u "Vec<_>"))
(rule (empty-test e x)
  (expr e "$x.len() == 0")
  (type-of x t)
  (peel-refs t u)
  (type-matches u "String"))

(rewrite is-empty (empty-test e x)
  (not (in-macro-expansion e))
  (replace e "$x.is_empty()"))
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
  atom in a blaze program is a load error that names the pattern forms. A pattern is parsed as
  edition 2024 reads it; searched over a file of an edition that reads its text as other Rust
  (`async {}` is a struct literal in 2015), it is refused, naming the rules line, both editions
  and the file. A file without the pattern's literal is still walked for its macro expansions,
  which another crate's macro can make spell it.
- **Bounded patterns.** A pattern or a template of more than 2048 non-trivia tokens is refused
  before it is parsed (a pattern at load, a template when its rule renders), and a pattern
  whose match would recurse more than 128 levels (one a tree level, one more for each `$$$run`
  before a node among its siblings) is refused at load. Each names the rules line. Siblings are matched in a loop, so a long argument list costs no stack.
  On a 2 MiB thread a debug build overflows the matcher at depth 690 to 710 and the parser at
  about 5700 tokens of nested generic arguments.
- **Paths match by resolution.** A multi-segment path in a pattern is a def path from a crate
  name (`core::mem::take`, `::std::sync::Mutex`) and matches a path in the code only when that
  path resolves to the same item: through an alias, a glob or a re-export, and never another
  crate's item of the same name. An associated item's path (`std::sync::Mutex::new`) resolves
  through its type's impls or its trait's items. A single-segment name matches by text, prelude
  names included (it is a local, a field or a method name, whose meaning `resolves` and `callee`
  give); write the def path to match by resolution. A pattern path that names nothing is a load
  error.
- **Rewrites.** `(replace n "tmpl")`, `(delete n)`, `(insert-before n "tmpl")`,
  `(insert-after n "tmpl")`, `(rename n "new_name")` (n a def; every reference moves, through
  blaze-intent), `(retarget n "krate::path")` (n a path node; written as the file sees the new
  item. A bare name is written as the new item's name and the item is imported: beside the
  private `use` that brought the old item, which goes once no read the request leaves remains,
  or else at the end of the module's `use`s. A retargeted `use` leaf is that import, moved the
  same way. Refused, naming it: a new name that already means something else there, a
  retargeted `use` a read still needs, a `pub use`, a use-tree prefix or glob), `(add-use n "krate::path")` (n any node of the file), and
  `add-param`, `remove-param` and `reorder-params` (d a fn; every call changes with it; see
  "Changing a fn's parameters"). Templates
  splice `$var`; `$$` is a literal `$`. A template is parsed with each metavariable as a hole,
  as the form of what it edits (an expression it replaces, or statements it replaces or sits
  beside in a body), and a bound expression is parenthesized only where its hole's precedence
  needs it (`syntax::view::prec`); then the replacement is, where the edited expression's
  parent needs it: `$a * $b` with `$a` = `x + y` writes `(x + y) * b`, and `$a + $b` over the
  `f(a, b)` of `f(a, b).len()` writes `(a + b).len()`. A template that is not that form is
  refused, naming the rule. An absolute path in a template (`::core::mem::take`) is a
  def path, respelled as the edited node's module names that item (`Module::find_path`:
  `crate::new`, `new`, `std::mem::take`); no `use` is added. A template path that names nothing
  is an error naming the rule. A path inside a string, char or byte literal or a comment of the
  template is text and is left as written. A rewrite may hold several actions; the same edit
  from two rules is one edit, and so is the rename of one def asked by several rows.
- **Refusals, each naming what it names.** Two edits that overlap, two insertions at one offset
  included (both rules, their rules lines, the file and line). A match a macro made, whose node maps back to no range of the macro's input
  (the macro call's `path:line`, the rule), unless the rule excluded it with
  `(not (in-macro-expansion n))`. A rewritten file that no longer parses (the file, the first
  syntax error), and a file that had a syntax error before the rewrite (the file, the line, the
  error); the whole request fails, nothing is written.
- **Surfaces.** The API first, schema 62 (63 adds `Edit::Params`): `Edit::Rules { rules }` rides the existing
  `Request::Edit`, so the answer is the usual `Edited` (VFS ops and seeds, the way
  `blaze_intent::apply` sends them); one per request, after the request's other edits.
  `Request::Edit`'s `head` is optional: with none, the edits are made and answered and nothing is
  committed, which is a dry run. `Request::Query { view, rules }` is read-only and answers
  `Response::Rows` (`blaze_api::query`): each relation's name, columns and rows by place, each
  cell code (file, line, range, text), a node a macro made (the call's place, its text), a def
  path, a type, text or an integer; then each lint finding (relation, severity, message, its
  first code cell). The CLI beside `--replace` on `blaze check`: `--rules FILE` applies a rules
  file (with any `--replace`, after them), `--dry-run` with `--rules` or `--replace` prints the
  unified diff and writes nothing, `--query FILE` prints the rows (one JSON line of
  `Response::Rows` with `--message-format json`) and exits failed when an `error` lint found
  anything.

## The relations

Values are syntax nodes (a file tree or a macro expansion tree), defs, types, crates' names as
text, text and integers. "Needs" is the argument a relation is demand-driven on: a rule must bind
it from an atom to its left, and a rule that does not is a load error naming the atom and the
argument.

| Relation | Needs | Gives |
|---|---|---|
| `(expr n "pat")` and the other pattern forms | nothing | every matching node and its metavariables |
| `(syntax n "KIND")` | nothing | every node of that `SyntaxKind` |
| `(match-arm m a p b)` | nothing | a `match`, each of its arms, the arm's pattern and its expression; from `m`, from `a`, or every arm of the view. A guard is no part of `p` |
| `(child p c)`, `(descendant p c)` | `p` | each child node of `p`, or each node below it (never `p`), in order; `parent` and `ancestor` are their inverses |
| `(int-value n i)` | `n` | the integer an integer literal means, in any radix, with `_`s and a suffix (`0x1F` is 31), or a negated one in an expression or a pattern (`-2`); a value past `i64` has no row |
| `(str-value n s)` | `n` | the text a string literal means, escapes and raw strings read (`"a\tb"` is `a`, a tab, `b`); a byte or C string has no row |
| `(resolves n d)` | `n` | the def a path, name, method call or field names |
| `(def-path d p)` | `d` or `p` | the def path from the crate name; or the def at that path, a module included. A variant is under its enum (`lib::E::V`), a field under its struct, union or variant (`lib::S::field`, `lib::E::V::inner`, `lib::P::0`). An impl's path ends in `<impl Type>` or `<impl Trait<Args> for Type>`; it names the impl and resolves to nothing, so a `p` naming one is refused. An associated item is under its impl's ADT, or under the impl segment when the self type is no ADT (`lib::<impl Go for &'_ str>::go`) |
| `(def-parent d p)` | `d` or `p` | the def that owns `d`: a variant's enum, a field's struct, union or variant, an associated item's impl or trait, an item's or an impl's module, a module's parent (a crate's root has none); or each def `p` owns |
| `(variant-count e n)` | `e` | how many variants the enum has; any other def has no row |
| `(type-kind t k)` | `t` | the type's shape as text: `struct`, `enum`, `union`, `ref`, `ptr`, `slice`, `array`, `tuple`, `str`, `bool`, `char`, `int` (signed), `uint`, `float`, `dyn`, `param`, `closure`, `fn`, `impl`, `never`, `unknown`, else `other` |
| `(type-def t d)` | `t` | the ADT the type is, or the trait of a `dyn Trait`; a reference is not peeled, so `(peel-refs t u)` first |
| `(def-name d t)` | `d` | the def's own name as text, the last segment of its path (`0` for a tuple field); an impl has none, so no row |
| `(type-of n t)` | `n` | the type of an expression or a pattern |
| `(type-matches t "pat")` | `t` | holds when the type matches; `$name` in the pattern binds a type argument |
| `(peel-refs t u)` | `t` | `t` with every leading `&` and `&mut` stripped, or `t` itself |
| `(implements t "krate::Trait")` | `t` | holds when the type implements the trait |
| `(callee n d)` | `n` | the fn a call or method call runs, through parentheses around the callee and trait dispatch where the front end knows the impl |
| `(enclosing-fn n d)`, `(enclosing-impl n d)`, `(enclosing-module n d)` | `n` | the innermost such item |
| `(crate-of n c)` | `n` | the crate's name, as text |
| `(fn-param d i t)`, `(fn-return d t)` | `d` | a fn's parameter types by index (self is 0 when present), its return type |
| `(def-kind d k)`, `(visibility d v)` | `d` | `fn`, `struct`, ...; `pub`, `pub(crate)`, `private`, ... |
| `(attribute d "name")` | `d` | holds when the item carries that attribute |
| `(defines n d)` | `n` or `d` | the def an item's, a variant's, a field's (named or tuple), an impl's or a `macro_rules!`'s syntax declares; or that syntax |
| `(references d n)` | `d` | every name, path, method call and numbered field access (`p.0`) in the target files that resolves to `d` |
| `(in-macro-input n)`, `(in-macro-expansion n)`, `(in-unsafe n)`, `(in-test n)`, `(cfg-off n)` | `n` | filters: `in-macro-input` holds for a node of an expansion that came from the macro's input, `in-macro-expansion` for one the macro made; `in-test` for one under `#[cfg(test)]` or an attribute whose path ends in `test` (`#[tokio::test(...)]`); `cfg-off` for one under an item, variant or field whose `#[cfg]` removed it, and a `#[cfg]` on anything else in between is refused, naming it |

Kept from astlog: `ancestor`, `parent`, `precedes`, `text`, `kind`, `text-match`, `same-text`,
`same-file`, `file-path`, `misgrouped-digits`. New in the engine for every source: `(count c (rel
args...))` binds `c` to the number of distinct rows of a lower-stratum relation that agree with the
bound arguments, and `(lt a b)`, `(le a b)` compare integers; integer literals are bare numbers.
`(not (atom ...))` negates a source atom or a builtin as well as a relation: every variable bound to its left,
it needs no stratum, and it is how a rule excludes macro-made matches. `(not (text-match x "re"))`
holds when the regex finds nothing in `x`. `(count ...)` over a builtin is refused; a builtin is
no finite relation.

Text builtins take a node or text first and need it bound. A regex is a string literal,
compiled when the file is read; one that does not compile, or whose compiled program is past
1 MiB (`MAX_REGEX_BYTES`), is a load error naming the line and the pattern.

| Builtin | Needs | Gives |
|---|---|---|
| `(text-match x "re")` | `x` | holds when the regex matches somewhere in `x`'s text |
| `(text-capture x "re" g1 g2 ...)` | `x` | holds when the regex matches; binds each capture group of the first match, in order, as text (a group that took no part binds empty text). The variables must be one per group, checked at load |
| `(text-replace x "re" "with" y)` | `x` | always holds; `y` is `x`'s text with every match replaced. `$1`, `${1}` and `${name}` write a group, `$$` a literal `$`. `$name` takes the longest run of letters, digits and `_`, so `$1_new` would read a group `1_new`: a `$` that names no group of the pattern, or names nothing, is a load error that gives the `${1}` spelling |
| `(case x "style" y)` | `x` | `x`'s text as `snake`, `camel`, `pascal`, `upper-snake` or `kebab` (the style a literal). Words split at `_`, `-` and case changes (`HTTPServer` is `HTTP` `Server`, digits stay with the word before them); leading and trailing `_` are kept. Text with any other character, `r#` included, is an error naming it |

## Type patterns

A `type-matches` pattern is `_` (any type), `$name` (any type, bound; one name used twice binds
equal types), a def path from a crate's name with generic arguments on its last segment
(`std::vec::Vec<$a>`, `lib::Pair<$a, $a>`), a single name with or without arguments
(`Vec<$a>`, `u32`, `Bag`), `&T`, `&mut T`, `(T)`, `!`, a tuple, `[T]`, or `[T; 4]` and
`[T; _]`. An array's length is an integer literal, matched against the length the front end
evaluates, or `_`, any length; a length it cannot evaluate (`[T; N]` in a generic fn) matches
only `_`, and a pattern length of any other form (`N`, `$n`) is a load error. Arguments
match the type's first type arguments, so `Vec<$a>` is `Vec<T, A>` for any allocator; one left
out matches anything. A single name is resolved first. A builtin type's name (`u32`, `str`)
is that type. A name the std prelude of a view crate gives (`Vec`, `String`, `Option`,
`Result`, `Box`) is that item, looked up in the prelude module itself, so a crate's own
`struct Vec<T>` does not match `Vec<$a>`; write `mycrate::Vec<$a>` for that one. Any other
single name matches an ADT of that name by text, with its arguments. A form the matcher does
not read is a load error that says what to write instead: `impl Trait` and `dyn Trait` point
at `(implements t ...)`, a raw or fn pointer at `$t`, a path from `crate::` at the crate's
name.

## Changing a fn's parameters

```lisp
;; `lib::connect` takes a timeout; every call passes the old one
(rule (connect d) (def-path d "lib::connect"))
(rewrite connect-timeout (connect d)
  (add-param d "timeout_secs: u64" "30"))
```

`(add-param d "name: Type" "default")` appends a parameter to the fn `d`, or puts it before
the parameter of an index given last (`self` not counted), and passes `default` at every call.
`(remove-param d "name")` drops a parameter and its argument. `(reorder-params d "b" "a")`
names every parameter once, in the new order. Each is one `Edit::Params { item, params }`
(schema 63): the new list, each entry a kept parameter by its pattern text
(`Param::Kept { name }`) or a new one (`Param::Added { param, default }`). blaze-intent makes
it with the renames, before any other kind of edit, so it resolves in the view as it is.

Every name spelled like the fn, or like a `use ... as` alias of it, is resolved. A path call,
a method call, a UFCS call (`Trait::f(x)`, `<T as Trait>::f(x)`), a call through an alias or a
re-export, and a call in a macro's input whose expansion resolves to the fn get the change. A
trait's method changes in the trait, in every impl and at every call; asked of an impl's fn,
the change is refused, pointing at the trait. An added argument is inserted where it goes and
needs no parentheses: an argument is a whole expression, and the default must parse as exactly
one. A removal or reorder rewrites the argument list. A dropped or moved argument must be plain
(a literal, a path, or a reference to one), so no side effect is lost or reordered; in a
macro's input only additions are written.

Refused, each naming the file and line: the fn used as a value (`let g = f;`, `map(f)`), whose
callers it cannot see; a call a macro made, which has no input to write an argument in; a call
with an argument count other than the fn's (a macro that drops or adds arguments); a field
shorthand that is the fn's name; an `extern` ABI, `#[no_mangle]` or `#[export_name]` fn and a
C-variadic one, whose callers are outside Rust; a removed parameter the body still reads, or a
removed pattern parameter (`(a, b): (u8, u8)`); an index past the list, a name the fn has not,
and a reorder that does not name every parameter once.

## Computed renames

```lisp
;; every fn named `legacy_*` loses the prefix, and every reference with it
(rule (legacy d new)
  (syntax n "FN")
  (defines n d)
  (def-name d name)
  (text-capture name "^legacy_(.+)$" new))
(rewrite drop-legacy-prefix (legacy d new)
  (rename d new))
```

An action's text argument is taken as written: `(rename d "$new")` would name the text `$new`,
so a `$name` in one is a load error that says to pass the variable bare. The text builtins
compute it instead: `(text-replace name "^legacy_" "" short)` then `(case short "pascal" t)`.

## The evaluator

Semi-naive per stratum: each round evaluates each recursive rule once per body position that
names a relation of the stratum, that position reading only the facts new in the last round.
Relation atoms join through hash indexes on the columns bound at that position, built once per
(relation, bound columns) and kept up to date as rows arrive. Variables are slots, not names.
Each source atom in the program answers each bound-argument tuple once a run, recursive rounds
included. Stratified negation and `count`, every variable bound; a program with no
stratification is a load error naming the relations on the cycle, and so is a `count` whose
result variable is one it counts over. A rules file nested past 64 lists is refused, naming the
line of the list past it. A run holds at most 10,000,000 rows at once (`MAX_ROWS`: derived rows,
kept source answers and rows a body has made and not stored); past it the run stops, naming the
relation and its rule, the source atom, or the rewrite. The old naive evaluator stays as the
reference model for a property test only.

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
- Build selection for a rules file. `--query`, `--rules` and `--replace` read the daemon's view
  of the whole checkout and refuse `-p`, `--features`, `--target`, target selection and the
  other selection flags by name. The end state is a query that carries `Targets`, as
  `Request::Trace` does, so the view is the selected build's.
- Queries and rewrites in test configuration. A rules file runs over the view's one cfg set
  per crate, so code only a test build compiles (`#[cfg(test)]` items, `tests/` targets,
  dev-dependencies) is matched as syntax but, where that cfg is off, has no types or
  resolution, and a rename or a parameter change can miss its uses. The end state:
  `Request::Query` and `Edit::Rules` carry `Targets`, as `Request::Trace` does. The daemon
  lowers them to the units a build would compile, each with its cfg (`test` for a test target,
  the features, the target) and its dev-dependencies, opens one view per distinct cfg, and
  runs the rules over each. Rows are merged; the edits two views make must agree byte for
  byte, and a disagreement is refused, naming both units.
- Prelude resolution of single-segment names in code patterns; they match by text. Type
  patterns resolve them (see Type patterns).
- A `use` added for a template path `find_path` can only spell through an import; it is written
  as `find_path` spells it.
- Precedence outside expressions. A metavariable spliced as a type (`&dyn A + B`), a pattern
  (`&a | b`), a path segment or an item is written as its text, and so is a template that
  replaces any node that is not an expression or a body's statement.
- A query's cost report. `Response::Rows` carries no per-relation timing; `blaze trace` does
  not see the rules' evaluation.
- Patterns per edition. A pattern is parsed once, as edition 2024, and refused on a file of an
  edition that parses its text otherwise. The end state parses it once for each edition the
  view's files use.
- Templates per edition. A template is parsed as edition 2024 for precedence and template
  paths, whatever the edited file's edition.
- `cfg-off` under a `#[cfg]` on a statement, an expression or any node that declares no def. The
  front end keeps no def there to say whether it is on, so the rule is refused, naming the
  attribute and its place.
- Text rewrites of a file that already has a syntax error. No count or list of errors maps
  through edits, so such a file is refused whole. The end state checks that the edits add no
  error by reparsing only what they touch.
- Patterns and templates past 2048 tokens, and patterns past 128 levels of matching. They are
  refused. The end state is a
  matcher and a parser that keep their own stack, bounded by memory rather than by the thread's.
