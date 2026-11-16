---
status: plan
summary: The flow, one mid-level form per body (decision D61), and the move and borrow checks over it (crates/blaze-flow). Places, points and statements; move paths and the two initialization dataflows; origins, subset constraints and loans; the two tiers (a location-insensitive pass, then the precise one only when the first cannot accept); the per-body record and its proof; the questions to other layers; the floor; what is left out.
date: 2026-11-16
---
# The flow, and the move and borrow checks

Decision D61 is the skeleton and is not reopened here. This spec says what the flow is, what each
check computes over it and from which written rule. The interface crate is `crates/blaze-flow`:
types, the move check and both borrow tiers over hand-built flows, each proven by a simulation with
an oracle and a planted bug. The lowering from the typed body and the constraint generation from
types are the next legs.

## Sources, quoted

- Reference, Expressions: "A _place expression_ is an expression that represents a memory
  location." "These expressions are paths which refer to local variables, static variables,
  dereferences (`*expr`), array indexing expressions (`expr[expr]`), field references (`expr.f`)
  and parenthesized place expressions." "All other expressions are value expressions."
- Reference, Moved and copied types: "If the type of that value implements `Copy`, then the value
  will be copied." "Only the following place expressions may be moved out of: Variables which are
  not currently borrowed. Temporary values. Fields of a place expression which can be moved out of
  and don't implement `Drop`. The result of dereferencing an expression with type `Box<T>` and that
  can also be moved out of." "After moving out of a place expression that evaluates to a local
  variable, the location is deinitialized and cannot be read from again until it is reinitialized."
- Reference, Temporaries: "The drop scope of the temporary is usually the end of the enclosing
  statement." Destructors: "The temporary scopes for expressions in `let` statements are sometimes
  _extended_ to the scope of the block containing the `let` statement." "When control flow leaves
  a drop scope all variables associated to that scope are dropped in reverse order of declaration
  (for variables) or creation (for temporaries)." "Assignment also runs the destructor of its
  left-hand operand, if it's initialized. If a variable has been partially initialized, only its
  initialized fields are dropped."
- RFC 2094: "We say that a variable is live if the current value that it holds may be used later."
  "a lifetime L is live at a point P if there is some variable `p` which is live at P, and L
  appears in the type of `p`." Supporting prefixes are "formed by stripping away fields and derefs,
  except that we stop when we reach the deref of a shared reference." Shallow access: "references
  or pointers found within are not dereferenced"; deep: "all data reachable through a given lvalue
  may be invalidated or accessed by this action"; "writing `a` is legal if `*a` is borrowed,
  whether or not `a` is a shared or mutable reference." On drops: "the MIR instruction
  `DROP(variable)` is not treated like other MIR instructions when it comes to liveness"; all
  lifetimes in the type "except those marked as may-dangle are live."
- RFC 2025: "the value starts out as reserved and only becomes mutably borrowed when the resulting
  reference is first used"; while reserved "it acts exactly like a shared borrow – hence the
  borrowed value can still be read."
- Polonius book (rules/relations, rules/loans): `subset_base`: "the origin `origin1` outlives origin
  `origin2` at the point `point`"; `loan_killed_at`: "a prefix of the path borrowed in loan `loan`
  is assigned/overwritten at the point `point`"; "It is an error to invalidate a loan that is live
  at a given point." The rules, verbatim:
  `subset(O1,O2,P) :- subset_base(O1,O2,P).`
  `subset(O1,O3,P) :- subset(O1,O2,P), subset(O2,O3,P).`
  `subset(O1,O2,Q) :- subset(O1,O2,P), cfg_edge(P,Q), (origin_live_on_entry(O1,Q); placeholder(O1,_)), (origin_live_on_entry(O2,Q); placeholder(O2,_)).`
  `origin_contains_loan_on_entry(O,L,P) :- loan_issued_at(O,L,P).`
  `origin_contains_loan_on_entry(O2,L,P) :- origin_contains_loan_on_entry(O1,L,P), subset(O1,O2,P).`
  `origin_contains_loan_on_entry(O,L,Q) :- origin_contains_loan_on_entry(O,L,P), !loan_killed_at(L,P), cfg_edge(P,Q), (origin_live_on_entry(O,Q); placeholder(O,_)).`
  `loan_live_at(L,P) :- origin_contains_loan_on_entry(O,L,P), (origin_live_on_entry(O,P); placeholder(O,_)).`
  `errors(L,P) :- loan_invalidated_at(L,P), loan_live_at(L,P).`
  `subset_errors(O1,O2,P) :- subset(O1,O2,P), placeholder_origin(O1), placeholder_origin(O2), !known_placeholder_subset(O1,O2).`

No compiler's source or output is a source; every test expectation below is stated from these.

## 1. The flow's forms

- A **local** is a typed id. A **place** is a local and a chain of projections: `Field(i)`,
  `Deref` (of a reference or raw pointer), `DerefBox` (of a `Box`: owned, so moves through it are
  allowed, per the Reference), `Index` (any index: two indices may overlap), `Downcast(v)` to a
  variant. Places are interned in one flat table, each row its parent place and its last
  projection, so a place is a `Place` id and its prefixes are a walk up parents.
- A **statement** is one row of one array per body; a **block** is a range of that array whose
  last row is its terminator (`Goto`, `Switch` on a place with targets, `Return`, `Unreachable`).
  Successor lists are one flat array of block ids.
- A **point** is a statement's `Start` or `Mid`. Accesses (and so invalidations) happen at Start;
  kills take effect at Start after the access is checked; loans are issued and `subset_base` facts
  hold at Mid. `cfg_edge` is Start(i) to Mid(i), Mid(i) to Start(i+1), and Mid(last) to the Start
  of each successor's first row.
- **Statements**: `Assign(dest, value)` where a value is `Use(Copy p | Move p | Const)`,
  `Borrow { kind: Shared | Mut | TwoPhase | Shallow, place, loan }` (a reborrow is a borrow of a
  place through a `Deref`), `Aggregate(operands)`; `Call { args, dest }`; `StorageLive(l)`,
  `StorageDead(l)`; `Drop(p)`; `Activate(loan)` (a two-phase loan's first use). The
  `subset_base` facts are a table of `(statement, from, into)`, each at the Mid of the statement
  that makes it (a call relates its arguments to its destination at its own Mid), given directly
  in this leg and generated from types in the next.
- **No unwind edges.** The rebuilt path aborts on panic, so a call or a drop cannot leave the body
  sideways: both are plain statements, not terminators, and there are no cleanup blocks. That
  removes the unwind half of every graph and every "drop on unwind" path; when unwinding is added,
  `Call` and `Drop` become terminators with an unwind successor and nothing else changes.
- **From the typed body**: a `let` binding with an initializer is `StorageLive` then `Assign`; a
  block's end is `Drop` then `StorageDead` of its locals in reverse order of declaration (the
  Reference); a temporary gets a local whose `StorageDead` is at the end of its statement, or at the
  end of the enclosing block when `let` extends it (the Reference's extension rule); a `match` is a
  `Switch` on the discriminant, and a guard gets a `Shallow` borrow of each place the patterns
  read, so the guard cannot overwrite what was matched; a closure is an `Aggregate` of its
  captures, each a `Borrow` or a `Move` as the type checker decided; `?` is a `Switch` on the
  branch result, its break arm a `Call` of `from_residual` and a `Goto` to the return block;
  `a.m(b)` with `&mut self` is a `TwoPhase` borrow of `a`, the arguments, then `Activate` and the
  `Call` (RFC 2025).

## 2. Moves

- A **move path** is a place of the table; its children are the rows whose parent it is.
- Two forward dataflows over bitsets of places, join = union: **maybe-initialized** and
  **maybe-uninitialized**. At entry the arguments are initialized and every other local is
  uninitialized. `Move p`, `Drop p` and `StorageDead l` / `StorageLive l` make the place and its
  descendants uninitialized (and not initialized); `Assign` and a call's `dest` make them
  initialized.
- **Findings** (exactly three): (1) **use of a moved value**: a read, move or borrow of `p` (not
  a `Drop`: a scope's end after a move is what drop elaboration resolves) where `p`, a prefix of `p`, or a descendant of `p` may be uninitialized (the Reference's
  "cannot be read from again until it is reinitialized"; a descendant covers a partial move);
  (2) **move out of a borrow, an index or a `Drop` type's field**: `Move p` where `p`'s chain holds
  `Deref` or `Index`, or a `Field` of a type that implements `Drop` (the Reference's list of what
  may be moved out of); (3) **assign to a moved-from part**: `Assign` to `p.f..` where a proper
  prefix of the destination reached without a `Deref` may be uninitialized (a prefix reached
  through one is a use of the moved pointer, finding 1). Every prefix is tested, none stopping
  the walk: each finding means "on some path", and the simulation found that stopping at the
  nearest prefix hides a use on another path.
- **Drop elaboration** reads the same result at a `Drop p`: initialized and not uninitialized is a
  static drop; not initialized is no drop; both is a drop behind a flag; a descendant uninitialized
  is an open drop of the initialized fields (the Reference: "only its initialized fields are
  dropped"). One function, `moves::drop_kind`, so the checks and the builder agree by construction.

## 3. Origins and constraints

- Every reference position in a local's type gets a fresh **origin**; a type's lifetime
  parameters get one origin each, read from its head's lifetime part; a signature's own lifetime
  parameters are **placeholders**, live everywhere.
- **Variance** (a record per type in its head's lifetime part) says the direction: covariant
  `subset(o_src, o_dst)`, contravariant the reverse, invariant both.
- An **assignment** `d = v` relates `v`'s type to `d`'s type at the statement's Mid. A **reborrow**
  `&*r` with `r: &'a T` adds `subset('a, o_loan)` for each `Deref` in the supporting prefixes
  (RFC 2094). A **call** instantiates the callee's signature with fresh origins, relates each
  argument to its parameter and the return to `dest`, and adds the callee's outlives bounds as
  subsets.
- **Liveness**: a local is live where its value may be used later (backward dataflow); an origin is
  live at a point when a local live there mentions it, or it is a placeholder. At Start(s) the
  locals are live-in of s; at Mid(s), during s, live-in or live-out, so a call's argument origins
  are live where the call relates them to its destination. A `Drop` makes only
  its drop-live origins live: all but the `may_dangle` ones (RFC 2094); this leg treats every
  origin of a dropped local as drop-live, which refuses more and never less.

## 4. Loans

- A loan is issued at a `Borrow`'s Mid into its origin. An assignment to a prefix of a loan's place
  kills it at Start (Polonius `loan_killed_at`), after the assignment's own access is checked.
- An access is a place, read or write, deep or shallow. `Copy` reads deep; `Move`, `Drop` and a
  `Mut` borrow write deep; `Shared` and `TwoPhase` borrows read deep; `Shallow` reads shallow; an
  assignment's destination and `StorageDead` write shallow; `Activate(l)` writes deep, except
  against `l` itself. A loan is **invalidated** by an access whose place overlaps it (one place a
  prefix of the other, distinct fields or variants disjoint, indices overlapping; a shallow access
  or loan does not reach through a `Deref`) when the access writes, or reads and the loan is `Mut`
  or an activated `TwoPhase`.
- **Two-phase** (RFC 2025): a `TwoPhase` loan is reserved, acting as shared, until `Activate`;
  "activated" is a forward dataflow (gen at `Activate`, kill at the reissue).
- **The error condition** is Polonius' `errors`, and `subset_errors` for placeholders not related
  by the signature's known bounds.

## 5. The two tiers

- **Tier two is the definition**: the Polonius rules above, evaluated as stated over the points,
  one bitset of subsets (origins squared) and one of contained loans (origins times loans) per
  point, to a fixed point on a work list. Its bound: each pop adds a fact or is a first visit, so
  pops are at most points plus points times (origins squared plus origins times loans).
- **Tier one** drops the location: one global subset graph (every `subset_base` fact, at any
  point), contained loans per origin closed over it once (a work list over origins, at most edges
  times loan words), kills ignored; liveness is kept. A loan is live at P if an origin that may
  contain it is live at P. Linear in points plus constraints, times loans over 64.
- **The obligation**: every fact tier two derives is derived by tier one (its `subset` and
  `origin_contains_loan_on_entry` drop the point and the kill), so tier one's errors include tier
  two's: **if tier one accepts a body, tier two accepts it**. The simulation checks it on random
  flows. Tier two runs only for a body tier one refuses.

## 6. The record

- **Key**: the body's check key (its text and what its names resolve to) plus the lifetime parts of
  the heads it read (signatures' lifetime parameters, outlives bounds, variances). Nothing else:
  types stay erased in every other key.
- **Content**: the findings, sorted by point, each naming its place, its kind and its statement's
  item-relative range; and when there are none, a `Checked` proof value the builder takes.
- An edit to a body's text rechecks that body; an edit to a callee's signature rechecks only the
  bodies whose key read that head's lifetime part.

## 7. Questions to other layers

- Type checker: (a) the type of each local and each temporary, as an erased type with its
  reference positions numbered: `[(local, positions: u32)]`; (b) the adjustments per expression
  (auto-ref with mutability, auto-deref steps, `Box` derefs): `[(expr, [Adjust])]`; (c) each
  closure's captures: `[(closure, [(place, ByRef(Shared|Mut|Unique) | ByMove)])]`; (d) each call's
  callee and whether it is a method with which receiver mode: `(expr, Def, Receiver)`; (e) which
  types are `Copy` and which implement `Drop`: `(ty, bool, bool)`.
- Heads: the lifetime part: `{ params: [Lifetime], positions: [(param, position)], bounds:
  [(a, b)], variance: [Co|Contra|In] }`.
- Layout: nothing.

## 8. The floor

- Tier one: one backward liveness pass and one forward activation pass over the points, a bitset
  of locals (locals / 64 words) per point, and a bitset of loans (loans / 64 words) per origin.
  The move check: two forward passes, places / 64 words per point for each set.
- Almost every body has under 64 locals, places and loans: one word per point per set.

## 9. Not in this design

- Raw pointers are not tracked (a `Deref` of one is an access to the pointer only); `unsafe`
  code's rules are the unsafe checker's and Miri-style validation's.
- `static mut`: an access is an unsafe-checker matter, not a loan.
- Union fields: all fields overlap; the place comparison treats a union's fields as one place
  (a flag on the place row, next leg).
- Async bodies are checked before the coroutine transform, as plain bodies with `yield` as a
  `Call`; loans live across a yield are a finding of the coroutine layer.

## 10. The lowering (crates/blaze-flow-lower)

`lower(body: &BodyView, ck: &Checked, facts: &impl Facts, alloc) -> Result<Lowered, Refusal>`:
one type-checked body in, its flow out, or one typed refusal naming the form and its
item-relative range. A body is lowered whole or not at all.

What it reads from the type checker (crates/blaze-typeck, lifetime-erased; read only):
- `BodyView` (`body.typeck.view(body.tys.view())`): the expressions, patterns, statements and
  params as the checker holds them, each with its range.
- `Checked`: `expr_ty`, `pat_ty` (after peeling), `local_ty` (the binding's type); `adjusts`
  (`(ExprId, AdjustKind, PTy)`, sorted by expression: `NeverToAny`, `Deref { overloaded }`,
  `Borrow { mutbl, two_phase, raw }`, the pointer casts, `Unsize`); `callees` (`(ExprId,
  ItemKey, generics)`); `expr_defs` and `pat_defs` (`Def`: local, item, ctor); `fields` (`(ExprId,
  variant, field index)`); `captures` (read only to refuse closures); `types.view()`'s `PNode`s.
- What the record lacks comes through the `Facts` trait, answered from the heads store in the
  census: per ADT `{ copy, drop_impl, needs_drop, lifetimes, union, is_enum, is_box }`; per callee
  `{ writes_lifetimes, self_ref }`; field index by name and field count per variant; whether a type
  parameter is bounded by `Copy`.

What it reads from crates/blaze-build/src/walk.rs: nothing; walk.rs is the code builder's walk of
the same body, and it duplicates the flow's move analysis: E0382 at `moved()`, per-field init
bits (`field_bit`) and drop flags, and `needs_drop` by the layout's repr. After the switch the
builder takes the flow's `Checked` proof and its `drop_kind` per `Drop`, and its own copies go.

Refused by name (`Form`): closures, async blocks, `.await`, coroutines, unions, `static mut`,
struct update `..base`, destructuring assignment, bindings under or- and slice patterns, a
peeled reference under an ADT field pattern that binds (`PeelUnderAdtField`, question T1), a
binding mode that disagrees with `local_ty`, a guard that moves a binding, a call whose callee
writes lifetimes (question H1), a call where elision does not decide (`ElisionAmbiguous`), and
inline asm through `Missing`.

Origins from types: one origin per reference position, per ADT with lifetimes (one for all its
lifetimes until H1), per opaque, alias or dyn; a body's own signature takes the elision rules
(the Reference, "Lifetime elision": one input position gives the output's; `&self` gives
self's); a signature that writes lifetimes takes every pair of its placeholders as related (a
stand-in, counted as `sig_writes_lifetimes`). Calls: elided callees relate the output to the one
input position (or self); a callee with generic arguments carrying origins is unified through
one hub origin (`calls_unified`): sound for refusal, too strong for acceptance, counted.

Named questions (their answers' shapes):
- T1 (type checker): pattern peel adjustments: `[(PatId, peeled: u32)]`, the references
  default binding mode peeled before each pattern.
- T2 (type checker): per type `Copy` and `Drop`: `[(PTy, copy: bool, drop_impl: bool)]`, and
  per type parameter whether its bounds give `Copy`.
- H1 (heads, "Front end heads, ITEMS"): the lifetime part of each fn and ADT head: `{ params:
  [Lifetime], positions: [(param, position)], bounds: [(a, b)], variance: [Co|Contra|In] }`;
  until then the census reads a `'` in the head's text, a stand-in named in its doc.

## What is built (crates/blaze-flow)

- `flow`: the forms above as flat tables, `FlowBuilder`, and `Flow::print`'s stable text.
- `moves`: the two dataflows, the three findings, `drop_kind`. Proven equal to a path oracle
  (every path, each block entered at most three times, one concrete flag per place) on random
  flows; the control plants a join that intersects (`Plant::MovesIntersect`) and the oracle
  answers "move check missed".
- `loans`: accesses and conflicts, liveness, activation, `tier_one`, `tier_two` (each Polonius rule
  a marked step). The simulation checks section 5's obligation on random flows; the controls plant
  a tier two whose second contains rule runs backwards (`TierTwoReversed`) and a tier one that does
  not close over the subset graph (`TierOneUnclosed`), each answered "tier one accepted what tier
  two refuses".
- Fixtures: the Reference's moves (moved local, reinitialized, fields, `Box`, a borrow, a `Drop`
  type's field, assignment into a moved part, drop kinds); RFC 2094's problem cases #1 and #2
  (tier one accepts), #3 and #4 (tier one cannot accept, tier two accepts: #3 by location, #4 by
  `loan_killed_at`), and a live mutable loan read through a shared borrow (both refuse; one finding).
- Not yet: may_dangle, union fields, the per-place `Copy` and `Drop` facts from the type checker;
  tier two is dense (bits per point); a sparse form for large bodies when one is measured.

## For a successor

Next: the lowering from the typed body (needs the questions in section 7) and constraint
generation from types with variance; then real bodies from the census, measuring how many bodies
tier one accepts (the claim "almost every body" is not yet measured).
