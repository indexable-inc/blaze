---
status: plan
summary: "The Lints stage of the engine (plan row E1, D135): one task an item that has lintable tokens, every selected token rule in its one walk, keyed by the item's text, the edition and the rule set, reading nothing; its record the findings with fixes and a deterministic per-rule cost. Typed lints stay in the body check. An edit relints only the items whose text it moved, so lints cost what the check costs."
date: 2026-11-17
---
# The Lints stage

The owner wants lints as fast as the check: an edit reruns only the lints of what changed
(D135). This is that stage on the engine (engine.md), after `Bodies`.

## The task

One `Lint` task a lint unit: unit 0 is the file, its top-level items left out as holes; unit
`i + 1` is outline entry `i`, its children left out (blaze-lint units.rs). Every item is a unit,
so every item with lintable tokens has its task; a unit's own bytes are lexed once and each token
goes to the rules registered for its kind or word, every selected rule in that one walk (blaze-lint
engine.rs, already one pass before this stage). The outline lists each unit as a part whatever the
step, so the outline never depends on the rule set. No rule selected (`Step::Rust`, no
`lints-add`): no task is planned.

## Its key

Name: the file's subject and the entry's chain (lints.rs `unit_name`), so it stands while other
items move. Source: the unit's own bytes, its holes (item-relative), its scope's key and its
`variant-count` read (lints.rs `unit_source`), then the selected rule set's id (`RuleSet::id`:
each rule's id and version, the renames table) and the edition. A step change relints every unit
and nothing else; an edit inside `g` that keeps its length relints `g` alone.

## Its reads

None for token lints. The scope a unit is linted in is its parent's text, and that is in the
source (`Scope::key`). A rule that needs another item would read it by cell, as a body does.

## Its record

- Findings: rule number (`LINT_BASE + id`), range from the unit's start (rule 71), the words, and
  in the payload each fix's edits. No level (rule 81): the printer takes it from the step.
- The out is a digest of findings and words alone: a comment that keeps every range makes equal
  bytes, so the task is counted identical and nothing downstream moves.
- Cost: the unit's tokens (the floor every rule shares) and each rule's sites tried, counted at
  each dispatch into a rule (a word, a `[`, a comment, a sequence end, a fact probe). A count of
  the bytes, never a time, so two builders write equal records (rule 25). The profile prints
  `lints: N tasks, F findings; by rule: <rule> <cost> <findings>, ...`.

## Typed lints: a second walk

`args-inline` and `unsafe-op-in-unsafe-fn` need types, so they run inside the body check and file
their findings with its record. An item gets two walks, not one, because the token walk is keyed by
text alone: folded into the body check it would rerun whenever a body's read moved (a callee's
head, a name's binding), which is most body entries, and the token rules would cost what typing
costs. Two walks cost one extra lex of the item's bytes; one walk would cost every token rule on
every body re-entry.

## Suppressions

`#[allow(blaze::rule)]` and `#[expect(..)]` are tokens of the unit or of an ancestor's text, so the
task reads them with no read list: an ancestor's are in the scope the parent's pass gives, which is
in the key. The record keeps each suppression with the findings it suppressed; one that suppressed
nothing is itself a finding (`unused-suppression`), as rustc's `unfulfilled_lint_expectations`.
Built today: allocs.rs's `expect` of `blaze::implicit_allocator`; the general form is not.

## Not in this piece

Whole-crate lints (a rule over every item of a crate is a task of the crate, keyed by its items'
outs), applying fixes (`--fix` reads the payload), and the formatter.
