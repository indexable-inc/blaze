---
status: current
summary: "blaze's lints: crates/blaze-lint's one pass over an item's own tokens (units with holes, scopes, findings as columns, fixes by a pure function), the rule sets a project opts into (`rust`, `suggest`, `edition`, `strict`, `lints-add`), and the Lints stage of the engine (D135) in blaze-cli: one task a file, a unit relinted only when its key moved, its record the findings with fixes and a deterministic per-rule cost; typed lints in the body check; levels at print time; `blaze check --fix`."
---
# Lints

The owner wants lints as fast as the check: an edit reruns only the lints of what changed
(D135). `crates/blaze-lint` is the pass; the Lints stage of the engine, after `Bodies`, runs it (`crates/blaze-cli/src/engine_lints.rs`, `crates/blaze-cli/src/lints.rs`).

## The model

- **Unit.** A lint runs on one outline entry. Its input is the entry's whole text, with its
  children's ranges given as *holes*, plus the `Scope` its parent's pass gave it. The root unit is
  the file, with its top-level items as holes. Only the bytes between holes are lexed, so each
  byte of a file is lexed once. Lines are read in the full text, which lets a comment above a site
  be found even when it belongs to the parent.
- **Scope.** A scope holds:
  - the file's facts: test file, binary root, build script;
  - what the parent's text says about the child: test code around it, the innermost block kind,
    whether the child is associated, whether it sits in an inline `mod`;
  - the `Context`, meaning the `// clone:`, `// SAFETY:` and `// recursion:` walks that reach
    above the child's first line or past its last.

  The parent computes the scope (`Linted::children`). `Scope::key()` packs it into a `u64`.
- **Finding** (rule 81): the template, an item-relative range, its arguments, and its fixes. The
  template names the rule, which has a stable id, a name and a version. Arguments are a range or a
  table row, never text. Each fix is a list of `(range, replacement)` edits. A finding has no
  level.
- **Columns.** `Findings` stores findings as columns in the caller's `blaze_own::Alloc`. A pass
  allocates per unit, never per token. It has no recursion and is `no_std`.
- **One walk.** Each token goes to the rules registered for its kind or word, every selected rule
  in that one walk (blaze-lint `engine.rs`).

## Rules

`blaze_lint::Rule` names every rule. `Rule::HOUSE` is the ten text rules ported from blaze's
scripts with the same matches and exemptions: `casts`, `clones`, `allocs`, `untyped`, `recursion`,
`stops`, `ambient`, `unsafe-comment`, `format-macro`, `renamed`. Only `renamed` has a fix.

**What `renamed` fixes.** It fixes a call to an old blaze-own name in two cases:
- the receiver is a local, and every binding of it in the enclosing function before the call is
  one of these:
  - a typed `let` or a parameter whose type is `AVec`, `ADeque`, `ASoa`, `FixedVec` or
    `blaze_own::*`;
  - a `let` from such a type's constructor or from `vec_in!`;
- the call is a path call through such a type (`AVec::insert(..)`).

`contains(x)` becomes `scan_for(x).is_some()`. Any other receiver is reported for a person
(without a fix) when the name is one only blaze-own has (`insert_many`, `try_insert_many`). It is
not reported at all for `insert`, `remove`, `contains` and `position`, which std's maps, sets and
slices also have.

## Which rules a project gets

Every house rule encodes a choice blaze made for its own code (an explicit allocator, no ambient
`std`, no formatting macros, typed errors), not a defect of Rust, so a plain Rust project gets
none by default. It names a step; `blaze_lint::Step` is the one home of the mapping:

| step | rules | level |
|---|---|---|
| `rust` (the default) | none: only what Rust itself refuses; no lint task is planned | — |
| `suggest` | `Step::SUGGEST`: `unsafe-comment`, `untyped`, `recursion`, `variant-count` | advice |
| `edition` | `Step::EDITION`: those, `stops`, `casts`, `clones` and the rules the blaze edition makes errors later | advice |
| `strict` | `Rule::HOUSE` and `variant-count` | blaze's levels |

Where it is written: `lints = "<step>"` under the root manifest's `[workspace.metadata.blaze]`
(`Step::from_name`), and `lints-add = ["ambient-heap", ..]` for single rules on top; a value that
names no step is a finding on that line, never a fallback to the default. blaze itself is `strict`.

## The task

One `Lint` task a file, over its lint units: unit 0 is the file, its top-level items left out as
holes; unit `i + 1` is outline entry `i`, its children left out (blaze-lint `units.rs`). The
outline lists each unit as a part whatever the step, so the outline never depends on the rule
set. A unit whose name and key the file's last lint record lists (under the same tool, rule set
and edition) is valid by its key: its findings are copied, it is not linted again; that record is
a memo, not a read.

## Its key

A unit's name: the file's subject and the entry's chain (lints.rs `unit_name`), so it stands while
other items move. Its source: the unit's own bytes, its holes (item-relative), its scope's key and
its `variant-count` read (lints.rs `unit_source`), then the selected rule set's id (`RuleSet::id`:
each rule's id and version, the renames table) and the edition. The file's task is named by its
file (lints.rs `file_lints_name`), its source the file part over every unit's name and key
(lints.rs `file_part`). A step change relints every unit and nothing else; an edit inside `g`
that keeps its length relints `g` alone.

## Its reads

None for token lints. The scope a unit is linted in is its parent's text, and that is in the
source (`Scope::key`). A rule that needs another item would read it by cell, as a body does.

## Its record

- Findings: rule number (`LINT_BASE + id`), range from the unit's start (rule 71), the words, and
  in the payload each fix's edits. A clean unit holds no row. No level (rule 81).
- The out is a digest of findings and words alone: a comment that keeps every range makes equal
  bytes, so the task is counted identical and nothing downstream moves.
- Cost: the unit's tokens (the floor every rule shares) and each rule's sites tried, counted at
  each dispatch into a rule (a word, a `[`, a comment, a sequence end, a fact probe). A count of
  the bytes, never a time, so two builders write equal records (rule 25). The profile prints
  `lints: N tasks, F findings; by rule: <rule> <cost> <findings>, ...`.

## Typed lints: a second walk

`args-inline` and `unsafe-op-in-unsafe-fn` need types, so they run inside the body check
(`crates/blaze-typeck/src/rules/args_inline.rs`, `unsafety.rs`) and file their findings with its
record. An item gets two walks, not one, because the token walk is keyed by text alone: folded
into the body check it would rerun whenever a body's read moved (a callee's head, a name's
binding), and the token rules would cost what typing costs.

## Suppressions

`#[allow(blaze::rule)]` and `#[expect(..)]` are tokens of the unit or of an ancestor's text, so the
task reads them with no read list: an ancestor's are in the scope the parent's pass gives, which is
in the key. The one `expect` read today is allocs.rs's, of `blaze::implicit_allocator`.

## Printing

`Levels::of(rule)` comes from the step and is read at print time. The printer adds the item's
file offset to each range, and resolves `file:line:col` only then (rule 71).
`show_message(rules, item_text, findings, finding, &mut put)` writes the message. Within a fix,
edits are sorted by start and do not overlap. Each finding has at most one fix.

## `blaze check --fix`

A quiet check, then the first fix of each chosen finding turned into file ranges and applied by
`apply(text, edits, alloc)` (blaze-lint `fix.rs`), then a second check. `apply` is pure, and
refuses overlapping edits, out-of-range edits and edits that split a character with a typed
`FixError`. It never merges or drops an edit.
