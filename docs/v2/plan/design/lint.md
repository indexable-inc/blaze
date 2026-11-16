---
status: plan
summary: blaze's lints at item grain (crates/blaze-lint): one pass over an item's own tokens, findings as records keyed by item hash, rule set and scope, levels and positions at print time, fixes applied by a pure function
date: 2026-11-16
---
# Lints at item grain

`crates/blaze-lint` is built and tested; the driver and CLI wiring below is not.

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

## Rules

There are ten text rules, each ported from its script with the same matches and exemptions:

| id | name | fix |
|---|---|---|
| 1 | casts | none |
| 2 | clones | none |
| 3 | allocs | none |
| 4 | untyped | none |
| 5 | recursion | none |
| 6 | stops | none |
| 7 | ambient | none |
| 8 | unsafe-comment | none |
| 9 | format-macro | none |
| 10 | renamed | yes, see below |

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

## The record

- **Key:** `(item hash, RuleSet::id(), Scope::key())`.
  - The item hash is the outline's `Item::hash`, which covers the item's whole text.
  - `RuleSet::id()` is a BLAKE3 digest of each enabled rule's id and version, plus the rename
    table.
  - The scope key covers what the rules read outside the item.

  All three are known before the work runs (rule 65).
- **Value:** the `Findings` columns for that unit, and the children's scopes. On a hit, the
  children's scopes come from the record, so a hit never has to be re-read.
- **Cost of an edit.** A change to one item changes the hash of each of its ancestors. Those
  re-lint only their own bytes; the holes are skipped.

## Which rules a project gets

Decided 2026-11-16. Every rule here is one of blaze's house rules: they encode choices blaze made
for its own code (an explicit allocator, no ambient `std`, no formatting macros, typed errors), not
defects of Rust. A plain Rust project chose none of them, so by default it gets none. It climbs
AGENTS.md's adoption ladder by naming a step; `blaze_lint::Step` is the one home of the mapping:

| step | rules | level |
|---|---|---|
| `rust` (the default) | none: only what Rust itself refuses; the driver makes no lint task | — |
| `suggest` | `unsafe-comment`, `untyped`, `recursion`: defect classes in any Rust (an `unsafe` with no stated assumption, an error a caller cannot match on, a stack depth set by the input) | advice |
| `edition` | those, and `stops`, `casts`, `clones`: what the blaze edition makes an error later | advice |
| `strict` | all ten | blaze's levels |

Where it is written: `lints = "<step>"` under the root manifest's `[workspace.metadata.blaze]`
(`Step::from_name`); a value that names no step is a finding on that line, never a fallback to
the default. blaze itself is `strict`.

Why not more by default: on tempo (docs/v2/plan/design/tempo.md) every rule together gives 6,920
findings on 816 files, 1,705 of them `format!` calls and 1,589 unexplained clones: noise to a
project that never asked for blaze's style. `suggest` gives 85, each worth a person's look.

## What the driver calls (blaze-drive)

1. Per file, compute the file's facts: test path, binary root, build script, and modules declared
   in test code. For the last one, an out-of-line `mod m;` entry whose `Linted::test_at_end` or
   scope is test marks `m`'s file.
2. Lint the root first: `lint(&rules, &Unit { text, holes, scope: Scope::file(..) }, frame, &mut findings)`.
3. Lint each entry in pre-order, with the scope its parent returned for it. Each unit is one task,
   keyed as above.
4. Store each unit's findings as one record.

The rules are cheap, so a task per item per rule set is enough. Findings whose level is `Error`
are printed before the answer, and the command fails. Advice is printed after the answer.

## Printing (blaze-report)

`Levels::of(rule)` comes from configuration and is read at print time. The printer adds the item's
file offset to each range, and resolves `file:line:col` only then (rule 71).
`show_message(rules, item_text, findings, finding, &mut put)` writes the message through
blaze-text. Code in the message is set in backticks. Within a fix, edits are sorted by start and do
not overlap. Each finding has at most one fix today.

## `blaze check --fix`

1. Lint each changed item.
2. Take the first fix of each chosen finding.
3. Turn the edits into file ranges.
4. Call `apply(text, edits, alloc)`. It is pure, and refuses overlapping edits, out-of-range edits
   and edits that split a character with a typed `FixError`. It never merges or drops an edit.
5. Write the file.
6. Lint again.

The tests show that the fixed rule then reports nothing, that the result still lexes, and that
every token other than the renamed ones is unchanged.

## A typed rule (not built)

A typed rule takes `Input::Typed`: the item's typed body record together with its text. Its key
adds that record's key. It answers what text cannot:
- whether a clone is of a `Copy` type or a refcount (including a field's type, which today is
  read only within the unit);
- the receiver's real type for `renamed`;
- the bare collection names that `ambient` reaches through imports.

## Where text rules differ from the scripts

These were measured on one snapshot of `crates/`. Counts are distinct lines, script against lint:

| rule | script | lint | why they differ |
|---|---|---|---|
| casts | 17974 | 17975 | A `#[cfg(test)]` match arm ends at its `}` here; the scripts mark whole lines. |
| clones handle | 863 | 817 | Fields declared in another item are not read here (the typed rule's job). Moved to unknown. |
| stops index | 22615 | 22589 | Each script reads through a comment to the character before a site, so a comment ending in a word followed by `[pat]` counts as an index. |
| stops panic | 1036 | 1041 | Same cause: a comment ending in `.` hides a `panic!` on the next line from the script. |
| stops unreachable | 815 | 819 | Same cause as panic. |
| ambient std | 14646 | 14686 | Same cause: a `std::` after such a comment is missed by the script. |
| allocs expect | 6 | 9 | Here the attribute is read to its `]`, across lines. |
| allocs global | — | — | Counted per occurrence here; equal per line. |
| ambient vec_box, is_std | 35097, 175 | not ported | They need the file's imports, which is the typed rule's job. |

Other differences:
- An `// alloc:` inside a string is not counted here.
- A nested `#![cfg(test)]` covers only its module here.
- The item-alone property holds for items that start their own lines (rustfmt's output).

## Speed

The floor is one lex of every byte. On 4,767 files (89 MB, 290,858 units) on one thread, the lint
took 492 ms against a lex of 161 ms, a ratio of 3.05. The pass with no rule enabled took 379 ms of
that, so the overhead is per unit (scratch allocation, one lexer start per run between holes), not
in the rules. The next step is a reusable scratch object across units, and that 3x ratio is what it
must bring toward 1.1.

## Not built

- Wiring into the driver, the CLI and the report.
- Typed rules.
- A configuration file for levels.
- `--fix` choosing among several fixes.
- Item-relative caching inside a running command.
