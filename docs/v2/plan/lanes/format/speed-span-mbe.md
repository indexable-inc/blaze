---
status: plan
summary: speed-span-mbe — the macro_rules matcher, fragment parsing and transcriber in crates/mbe brought to the v2 speed bar against rustc's macro_parser; benchmark, before, changes, after.
date: 2026-10-29
---
# speed-span-mbe: the `macro_rules!` matcher and transcriber

Paths: `crates/mbe/`. Every `macro_rules!` call in a build goes through it: the definition is
parsed once (`parser.rs`, `lib.rs`), each call is matched rule by rule
(`expander/matcher.rs`), fragments (`$e:expr` and the rest) are parsed by the front end's
parser (`lib.rs` `expect_fragment`), and the matched rule's template is transcribed
(`expander/transcriber.rs`). Expansions, matched arms and errors are exactly as before; no
public signature changed (`expect_fragment`, also called by `hir-expand`'s builtin macros,
keeps its signature and results).

## Benchmark

rustc's `rustc_expand::mbe::macro_parser` (`TtParser`): an NFA over matcher positions. The
pattern is flattened once per rule into `MatcherLoc`s (cached with the macro's rules); a
position (`MatcherPos`) is an index and an `Rc<Vec<NamedMatch>>` of its matches, shared between
forks and copied on write; the `cur_mps`/`next_mps`/`bb_mps` buffers are reused for every step
and every rule; only the first failure is kept. Fragments are parsed by rustc's parser straight
from the token stream. Its transcriber (`transcribe.rs`) keeps a frame stack and a result
stack of token vectors, one per delimited group.

## Before

From reading the code as `format-span` left it (kept as `expander/model/`):

- **Per matcher step**: every fork cloned a `MatchState` of about 300 bytes (the dot, a
  `SmallVec` stack of four iterators, an `Option<Box<MatchState>>` parent, an
  `Option<Arc<Separator>>` (an atomic increment), the cached fragment result) and copied its
  bindings: two new `Vec`s (`BindingsBuilder::copy` of the nodes and of the iterations). Leaving
  a repetition deep-cloned the boxed parent: one heap allocation per level per exit. Glued
  punctuation was decoded again for every position. Every error message was formatted
  (`format!`, a `String`) even when an earlier error was kept instead.
- **Per binding**: an `Rc<BindingKind>` (a heap allocation) pushed onto a `Vec`. The final
  bindings were built as one `FxHashMap<MetaVar, Binding>` per repetition iteration, a `Vec`
  per `Nested`, every name hashed on insert.
- **Per fragment**: `expect_fragment` converted every token left in the level to the parser's
  input, whatever the fragment's length: `$($e:expr),*` over `n` expressions converted
  `O(n^2)` tokens.
- **Per transcribed token**: each `$var` hashed its name into the bindings map. Each
  repetition iteration opened an invisible group and then removed it with `Vec::remove` at its
  index, shifting every token after it: `O(output)` per iteration, quadratic over a long
  repetition.

## What changed

- `parser.rs`: `resolve_binders` numbers the metavariables a rule's pattern declares, once
  per definition, and resolves every use in the pattern and the template to its number
  (`Binder`, a `u32`; `NO_BINDER` for one the pattern doesn't declare). Matching and
  transcribing never hash a name.
- `lib.rs`: a `Rule` holds its `Matcher`, flattened once in `Rule::new` (rustc's
  `MatcherLoc`, cached with the rule as rustc caches it with its lhses).
- `expander/matcher.rs`:
  - `Matcher`: `locs` (8 bytes each) with leaves in side tables by kind; descending into a
    group or repetition is `dot += 1`, no frame.
  - `Pos`: 24 bytes, `Copy`, in the `Arena`'s buffers, which one call reuses for every step and
    every rule. A fork is a 24-byte copy, no allocation. A repetition's parent is one copy into
    `Arena::ups`, not a boxed clone per exit.
  - Bindings are cons cells (`Entry`, 20 bytes) in one per-call arena, never changed once
    pushed, so any number of positions share them: binding is one push, forking copies two
    `u32`s (rustc copies its `Vec` on the first write after a fork).
  - The trees the transcriber reads are built once when the match is over (`Arena::build`):
    per-level slot arrays indexed by binder, then flat `Bindings` columns: `roots` per
    binder, `nodes` (`Binding`, 12 bytes), each `Nested` node's iterations a CSR range of
    `children`. No hashing, no `Vec` per repetition.
  - The token at the dot is decoded once per step, glued punctuation once per step for all
    positions, and an error is built only when it is a rule's first (`add_err_with`).
- `lib.rs` `expect_fragment`: the parser gets a window of the level's first whole top-level
  trees (8, doubled while needed), and the parse is kept once it provably read nothing past the
  window: the parser's position only moves forward and every read is within five tokens of it
  (`Parser::nth` asserts `n <= 3`, glued punctuation looks two further), so a parse that ended
  six tokens before the window's end saw the same tokens as one over the whole level. Every
  read at the end of input is guarded by a token-kind check, so a window can't make the parser
  fail where the whole level wouldn't. The cursor still walks the whole level, so the
  iterator's end is kept. A fragment costs its own length: `$($e:expr),*` is linear.
- `expander/transcriber.rs`: a variable's binding is `roots[binder]`, one index; a level of
  nesting is one CSR index. Each repetition iteration writes straight into the output builder;
  the invisible group and its `Vec::remove` are gone. The group was always the last subtree
  closed and was always removed, so the output is the same. The output builder is made with
  room for the template's ops and every bound fragment's token trees
  (`TopSubtreeBuilder::with_capacity`, on top of `tt`'s reused scratch buffers).

## After

| | before | after | rustc |
| --- | --- | --- | --- |
| fork a position | ~300-byte clone, 2 `Vec`s, an `Arc` increment | 24-byte copy | 16-byte copy, `Rc` increment |
| bind a fragment | an `Rc` and a `Vec` push | a 20-byte push into the call's arena | `Rc::make_mut` (a `Vec` copy when shared) |
| leave a repetition | a boxed deep clone | a 24-byte copy and one push | a `Vec` push |
| a step's token, glued punct | decoded per position | once per step | once per step |
| a later error | formatted, dropped | not built | not built |
| bindings for the transcriber | a hash map per iteration, `Vec` per repetition | flat columns, CSR | `MatchedSeq(Vec<_>)` per repetition |
| a `$var` in the template | a hash lookup | an index | a hash lookup |
| a repetition iteration | an `O(output)` shift | nothing | a result `Vec` per group |
| fragment `k` of `n` | converts `n - k` trees | converts `O(len)` trees | parses in place |

Where it is still behind rustc, and what it would take:

- A fragment attempt converts its window to a fresh `parser::Input` (five vectors) plus an
  `FxHashMap` edition cache in `syntax_bridge::to_matcher_input`, and the parser allocates its
  events. rustc's parser reads the token stream in place. Matching it needs a parser that reads
  token trees directly, or at least a reusable `Input` (a `clear()` in `crates/parser`, a
  `to_matcher_input_into(&mut Input)` in `crates/syntax-bridge`): outside this crate.
- The `Arena` lives for one call (it borrows the input), so a call's buffers grow from empty: a
  handful of amortized allocations per call, reused across its rules and steps.

## Tests

`src/differential.rs` checks the new matcher, fragment parsing and transcriber against the old
code kept in `expander/model/` (identical to the old files but for import paths). Each check
compares the expansion tree, the matched arm and the error, for every call style:

- a corpus covering every feature (repetitions nested and side by side, every separator kind,
  `?`/`+`/`*`, every fragment kind, `${count}`, `${index}`, `${ignore}`, `${concat}`, `$$`,
  `$crate`, unbound variables, rule ranking, error recovery, attr and derive rules), in three
  edition pairs, with and without a span marker;
- the corpus expansions themselves fed back to `expect_fragment`, so its input has the
  delimiter-free groups earlier expansions leave;
- seeded random macros (patterns with groups, repetitions up to three deep and every fragment
  kind; templates with metavariable expressions and wrong-depth variables) with generated and
  mutated inputs: 40 seeds always, 400 with slow tests;
- the benchmark's real-world macros and inputs (with slow tests);
- `expect_fragment` against the old whole-level version at every position of every input, for
  every entry point, including fragments of 1 to 200 top-level trees around the window sizes.

The size budgets (`Loc` at most 8 bytes, `Pos` 24, `Entry` 20, `Binding` 12) are `const`
assertions and a test. `tests.rs` and `benchmark.rs` are unchanged in what they check.
