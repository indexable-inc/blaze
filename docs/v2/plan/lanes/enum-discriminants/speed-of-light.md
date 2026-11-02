---
status: plan
summary: lane enum-discriminants' speed of light: the bound of an enum's discriminant table, a variant's discriminant and a value's variant, and how hir-ty's consteval/discriminants.rs reaches it until the first compile measures it.
date: 2026-10-30
---
# enum-discriminants: speed of light

The work is fixed by the enum: one evaluation per explicit discriminant (`= value`), which
dominates whenever there is one, and one step per variant to count the rest. Everything else is
the table's own cost, and the bound is that cost against those two.

## The table: `enum_discriminants(EnumId)`

One pass over the variants (`EnumVariants::with_explicit_discriminant`, read off the syntax, no
body lowered): an implicit variant is a compare against the run's room and an increment, a few
cycles; an explicit one is its own `explicit_discriminant` query. A run starts only at the first
variant, at an explicit discriminant or at an overflow, so the table is `r` runs, not `n` values:
16 + 4 bytes a run, inline for one run. Almost every enum is one run (no explicit discriminant)
and builds with no allocation. Finding duplicates (E0081) sorts the `r` runs by first value and
sweeps them once: \(O(r \log r)\), \(O(r)\) when the explicit values ascend, the common C-like
case. A 20,000-variant enum with one explicit value is 20,000 increments and two runs: tens of
microseconds against a parse of its source that takes milliseconds, well inside 10%.

rustc's own `discriminants()` iterates every variant and allocates nothing either, but its
duplicate check (`detect_discriminant_duplicate`) is quadratic in variants; this is \(O(r \log r)\).

## One variant: `const_eval_discriminant(variant)`

rustc's `discriminant_for_variant`: a binary search of the explicit variants'
indices (`explicit_discriminant_indices`, a memo read off the syntax), the one explicit
evaluation it starts from, and a checked add. With no explicit discriminant the search is over
nothing. It reads neither the table nor any explicit value after it, so `B = A as isize + 4`
needs no cycle; and the per-variant memo backdates, so a reader of one variant is invalidated
only when that variant's value changes. The bound is one memo hit; a miss is that plus
\(O(\log r)\).

## One value's variant: `variant_of(value)`

A binary search of the runs not failed, ordered by first value, then a subtraction:
\(O(\log r)\), no allocation (the allocation-budget test checks zero for every lookup of a
20,000-variant enum). It replaces scans that evaluated each variant until one matched,
\(O(n)\) queries per lookup. An enum with duplicates, already an error, scans its `r` runs for
the lowest variant with the value.

## Layout

`layout_of_adt` takes every variant's discriminant from the table's iterator: one pass over
the runs, no query per variant. It was a query per variant, each counting back to the nearest
explicit discriminant, \(O(n^2)\) for an enum with one explicit value.
