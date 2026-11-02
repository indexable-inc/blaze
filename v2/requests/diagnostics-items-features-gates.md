# Requests from diagnostics-items-features-gates

## Parser / syntax (`crates/parser`, `crates/syntax`): gated syntax blaze cannot parse yet

rustc parses each of these and gates it with E0658 (`compiler/rustc_parse`, gated in
`compiler/rustc_ast_passes/src/feature_gate.rs::check_crate`). Today blaze's parser errors
instead, so `checks/features/gates/pre.rs` has no node to gate. Once the syntax tree has a node,
add its arm to `pre::gates_of` (and `post.rs` where `PostExpansionVisitor` also gates it).

- `for await x in y` (async_for_loop)
- `const { ... }` as an item (const_block_items)
- contract attributes' `contract_requires`/`contract_ensures` (contracts_internals)
- `x.use` and `use || ...` (ergonomic_clones)
- `final fn` in traits (final_associated_functions)
- `reuse path;` items (fn_delegation)
- `pat if guard` in patterns (guard_patterns)
- `move(expr)` (move_expr)
- `mut ref x` / `mut ref mut x` bindings (mut_ref)
- `&pin const T` / `&pin mut T`, `pin` receivers (pin_ergonomics)
- `expr.match { ... }` (postfix_match)
- `expr.yield` (yield_expr, postfix form)
- `unsafe<'a> T` binder types (unsafe_binders)
- attributes on where-clause predicates (where_clause_attrs)
- `!Trait` bounds (negative_bounds)
- `!` patterns and arms with no body (never_patterns)
- `= value` defaults on tuple-struct fields and `unsafe` tuple fields (default_field_values,
  unsafe_fields)

> forwarded to format: `crates/parser` and `crates/syntax` are lane format's.

## Driver (`crates/blaze-diagnostics/src/check.rs`)

- Soft-gate warnings need a phase after `Phase::Late`: rustc writes stashed warnings at the very end
  and only when the session had no error at all (including late lint errors). The gates module
  gives them at `Late` and drops them itself when one of its own gates errors.
- `emit_fatal` support: `post.rs` marks the ForbiddenBound error ("bounds cannot be used in this
  context" with a trait bound) fatal and stops its own output; rustc also stops every later pass.

> forwarded to diagnostics: its own `check.rs`.
> applied by diagnostics: `Diag::stop()` marks an error fatal (`Diag::fatal`), and the driver writes nothing after the first fatal error in rustc's order. `gates.rs` marks the ForbiddenBound error with it. The soft-gate warnings are at the new `Phase::Stashed`, which the driver writes last and only when nothing written was an error, denied lints included (`check::unstashed`, with tests).
