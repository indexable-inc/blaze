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

- crates/blaze-flow-lower (section 10): the lowering. Four fixtures go through the real type
  checker (moved twice, moved once, move while borrowed, move after last use). Each has a control
  with a planted bug (`AllCopy`, `NoRelate`) that hides the finding, and each prints the findings
  it gets. Both crates use blaze-own (no `alloc`).
- The census (crates/blaze-ladder/tests/flow_census.rs; first run, flow-census-1.txt in
  port27/flow-1116), whose head line reads: "bstd: clean bodies 2116; lowered 1247; refused 869;
  tier one accepts 924; tier two accepts 9; findings 1323 (in 314 bodies)". Per lowered body in a
  debug build: lowering 57.7 µs, move check 118.8 µs, tier one 151.6 µs. Tier two takes 46.4 ms
  per body sent (323 sent): it is dense and is the floor's problem.
  Refusals by form: CallWritesLifetimes 377 (H1), UnknownAdt 322, Closure 130,
  ElisionAmbiguous 32, OrPatternBinding 3, PeelUnderAdtField 3, StructUpdate 2. Findings: 1180
  UnprovenOutlives and 143 Invalidated. No move findings.

## Census 1, by class

From flow-census-1.txt (port27/flow-1116): 1323 findings in 314 bodies, every one a false
positive. Classes by the text at the finding; "cause" is read from the lowering's code, and where
it says "supposed" no flow was printed to confirm it. Sources: the Reference says what a
signature means ("Lifetime elision", "Lifetime bounds", "Subtyping and variance"); what a body
may do with those lifetimes is RFC 2094 and Polonius as sections 3 to 5 state them: the design
is the source there, not the Reference.

| Class | Count | Example (body: the finding's words) | Cause | State |
|---|---|---|---|---|
| A tuple of two parameters | 874 | derived `eq`: UnprovenOutlives 50..66 `( self , other )` | INPUT: `aggregate` made every position of a tuple's operands equal through one origin, so `(self, other)` related the two parameters' lifetimes. Right: `(a, b): (A, B)` relates nothing between `A` and `B` (the Reference, "Subtyping and variance": a tuple is covariant in each field) | fixed: each element is related to its own field (`relate_at`) |
| A call with two reference arguments | 100 | derived `eq`: UnprovenOutlives 63..187 `$crate::cmp::PartialEq::eq(a_self, a_other)` | INPUT: a call of unknown or generic signature made every position of its operands equal. Right: a reference passed by value is covariant in its own lifetime (RFC 2094, "Subtyping") | fixed: `unify_call` lets a reference operand's first position only flow in |
| `Self` returned by a `&self` method | 64 | derived `clone`: UnprovenOutlives 41..51 `{ * self }` | INPUT: `return_place` gave every position of the output the receiver's lifetime; the Reference's elision assigns it to ELIDED output lifetimes only, and `Self`'s are the impl's | fixed when the lifetime part is read: `Self`'s positions are `Region::SelfTy` |
| Other returns, struct literals, `x.clone()` | 142 | `<impl Written>::of`: UnprovenOutlives 68..141 `File { io: io.clone(), name, .. }`; `<impl Cx>::io`: 133..160 `{ &self.world }` | INPUT (supposed): the same three causes through a struct literal (related as a call of unknown signature) and through returns of bodies whose signature writes lifetimes | census 2 decides |
| A live loan invalidated in a derived body | 131 | derived `fmt`: Invalidated 279..751 `{ let mut ra_expand_builder = ..` ; derived `eq`: 172..442 | INPUT (supposed): `b.field(..)` with `b: DebugStruct<'_, '_>` and no signature: the receiver's loan flowed through the call's one origin into `b`'s own lifetime position, live while `b` is | fixed only where the callee's lifetime part is read (the loan then flows to the result alone); else open |
| A live loan invalidated elsewhere | 12 | `<impl Entered>::enter`: Invalidated 259..275 `CURRENT.set(run)`; `<impl Run>::current`: 263..276 `CURRENT.get()` | not read; `CURRENT` is a thread-local or a static: supposed a borrow of a static taken as a local's | open |

Refusals (869): CallWritesLifetimes 377 (now lowered when the callee's lifetime part is read),
UnknownAdt 322 (the census now asks the store for each ADT before lowering), Closure 130,
ElisionAmbiguous 32 (the same as the first), OrPatternBinding 3, PeelUnderAdtField 3,
StructUpdate 2.

## What the signature must say (H1)

Per fn head, beside its erased types (crates/blaze-flow-lower/src/sig.rs, `SigLife`, is this
shape and the lowering takes nothing else):

- `regions: [Region]`: one per ORIGIN POSITION of each input in order, then of the output. A
  type's positions are counted in pre-order, a node's own before its children's: one per
  reference, one per lifetime parameter of an ADT (today ONE per ADT with any: see the ADT part),
  one per trait object (its bound, after the Reference's "Default trait object lifetimes"), one
  per opaque or associated type. A `Region` is `Static`, `Named(i)` (a lifetime parameter of the
  fn or of its impl, the impl's numbered first), `Anon(i)` (each elided input lifetime its own)
  or `Hidden(i)`; elided OUTPUT lifetimes are already replaced by the Reference's two rules.
- `ends: [u32]`: per input, where its regions end.
- `bounds: [(Region, Region)]`: every declared `'a: 'b`, the fn's and its impl's, where clauses
  included. Implied bounds are not wanted: the lowering derives them from the types.

Per ADT head: `lifetimes: u32` (how many it declares), their variances `[Co|Contra|In]`, its
declared bounds, and per field its regions in the ADT's own parameters (so a struct literal and a
field projection relate each field exactly). Per impl head: the regions of its self type, so
`Self` in a method is its impl's lifetimes and not a separate region.

Until the heads carry it, the census reads it from the fn's own TEXT (`sig::read_signature`: a
stand-in, said so in its doc). It refuses by name what it cannot line up and never guesses. Its
known gaps, each of which can only refuse a correct body, never accept a wrong one: `Self` is a
region apart from the impl's named lifetimes (`impl<'a> T<'a> { fn f(&self, x: &'a u8) -> ..`
sees `Self`'s lifetime and `'a` as unrelated); the impl's declared bounds are not read; an ADT
written with two different lifetimes is refused (`TwoLifetimesOneOrigin`); a type alias with
positions is refused (`Unaligned`).

## The signature in the flow (leg 2; written, see BUILD-STATUS.md for what compiled)

- Own signature (`Lower::signature`): one placeholder per region; each position of each parameter
  and of the return place takes its region's placeholder. Known among them, and nothing else:
  the declared bounds, `'static: everything`, and for every reference `&'r T` in a parameter or
  the output, `'x: 'r` for each position `'x` in `T` (the Reference, "Lifetime bounds": the type
  is well formed). The check then refuses any `subset` between two placeholders outside the
  closure of these (section 4's `subset_errors`), and any loan of a local that reaches a
  placeholder (placeholders are live everywhere, so it is live at the local's `StorageDead`).
- A call (`call_by_signature`): the callee's regions get fresh origins per call (`'static` is the
  body's); operands flow into their parameter's regions and the output's regions into the
  destination (a reference's first position one way, the rest both ways); the callee's declared
  and implied bounds are added as subsets. A callee whose lifetime part is not read keeps the
  older paths (elision by `CalleeFacts`, refusal when it writes lifetimes, one origin for all
  when its head is unknown or its generic arguments carry origins).
- Elided `&self` calls also flow every other position of the receiver into the output (before,
  `self.items.iter()` on `&'a Vec<&'b T>` left `'b` unrelated to the result: a missed error).
- Fixtures: crates/blaze-flow/src/tests.rs `signature_*` (ten, by hand-built flows: accept by
  the same lifetime, elision, a declared bound, an implied bound, `&self`, a reborrow through
  `&mut`; refuse the other argument's lifetime, a local's reference, the implied bound reversed;
  controls `Plant::KnownIgnored` and `Plant::AllKnown`); crates/blaze-flow-lower/src/tests.rs
  (nine through the real type checker: the reader's rules, each fix with the nearest wrong
  program, controls `NoKnown`, `TupleUnified`, `HubBothWays`).

## Census 3 (12:44, tree 11d224a93a, through mixcheck; census 2 never ran)

"bstd: clean bodies 2223; lowered 1756; refused 467; tier one accepts 1427; tier two accepts 126;
findings 536 (in 206 bodies)" (was: lowered 1247, refused 869, findings 1323 in 314). Signatures:
own lifetime part read in 1702 bodies; 3265 calls related through the callee's; texts not read:
ElisionUndecided 52, Unaligned 7, Token 5. Refused: Closure 198, CallWritesLifetimes 189,
ElisionAmbiguous 30, UnknownAdt 25, PeelUnderAdtField 9, StructUpdate 6, TypeNode 6,
OrPatternBinding 4. Findings: 309 UnprovenOutlives, 224 Invalidated, 3 moves. The goal (under
100) is NOT met. By method, none of them diagnosed from a printed flow yet:

- Invalidated in `fmt` 84 (derived, and hand-written `write!(f, ..)`): the `format_args!` path
  (`aggregate` with `fmt`, now related by `unify_call`) and `f.debug_struct(..)` builders.
- UnprovenOutlives 68 and Invalidated 66 in derived `eq`, at a `$crate::..` path call inside the
  match: 12 + 12 more in `cmp` and `partial_cmp`. Supposed: bindings under a peeled reference
  pattern have inexact places, so `unify_call`'s one-way rule does not apply to them.
- UnprovenOutlives in `fmt` 40, `new` 32, `clone` 28, `only` 15: struct literals with a field
  from a parameter (`File { io: io.clone(), name, .. }`, `Given { cx, .. }`) and `x.clone()`:
  a struct literal is still a call of unknown signature (needs H1's ADT part).
- `CURRENT.get()` / `CURRENT.set(..)` (a thread-local static): Invalidated, about 8.
- Three MOVE findings, new because their bodies are newly lowered: `Shares::split` (UseOfMoved
  at a struct literal), `Shares::whole` (MoveOut(Deref) at `*unsafe { Box::from_raw(..) }`: a
  `Box` deref lowered as `Deref`, not `DerefBox`), `Pool::run_now` (UseOfMoved at
  `(head.release)(job.0)`).

## Lifetimes are produced once and carried beside the types (D148, the coordinator's design, 2026-11-19)

MEASURED (tempo, 16,556 bodies): 1,895 bodies are not borrow-checked; about 1,400 of them because
the flow REBUILDS lifetimes from source text in its own readers (crates/blaze-flow-lower sig.rs and
sig/*, front2 lifetimes.rs, borrows.rs's per-body loop: about 6,000 lines) and lines them up with
the erased types by position. Two counters differed by design (`SigPositionsUnaligned`, 382
bodies); each lifetime-carrying feature had its own reader and its own refusal class. Rule 66's
outside review (2026-11-19) and three failed rounds on one conversion say: rebuild.

THE RULE: a declared type's lifetimes are produced ONCE, by the head lowering, in the same walk
that interns the erased type, as a REGION COLUMN: one slot per region position of the type, in the
type's own pre-order. Types stay erased (interning, keys and the trait solver do not change). The
definer stores the columns in its head record; every reader takes them from there. Nothing reads a
signature's text for lifetimes again. Body types carry no column: a body's regions are inferred by
the flow, which gives every position a fresh origin, as now.

One home (rule 30), built: crates/blaze-typeck/src/regions.rs (`Region { Static, Param(u32),
Late { binder, var } }`, `region_arity`, `slots`, `each_slot_node`, `RegionCol::lay`, `Numbering`,
`LifetimeParams`). `Param(i)` numbers the container's lifetimes, then the item's own in DECLARATION
order, then one per elided input lifetime; never by first appearance. Elision (the Reference's three
rules), the trait-object default and early/late-bound are decided once, by the definer; E0106 is a
head error.

THE CONTRACT every piece writes against (crates/blaze-typeck/src/region_facts.rs, piece L2 makes
the file exactly so; pieces written before it exists use these names):

```rust
/// What one head's lowering produced (pieces L1a, L1b fill it; L2 stores it).
pub struct HeadRegions<A: Allocator> {
    pub params: LifetimeParams<A>,
    /// A fn: laid over `FnPtr(inputs.., output)`; `child(k)` is input k, the last child the output.
    pub sig: Option<RegionCol<A>>,
    /// An async fn's WRITTEN return type (the future's output).
    pub written_output: Option<RegionCol<A>>,
    /// One per where-clause or bound, in the head's clause order (`nth`).
    pub clauses: Vec<ClauseRegions<A>, A>,
    pub outlives: Vec<Outlives, A>,
    /// An ADT: (variant, field, the field type's column).
    pub fields: Vec<(u32, u32, RegionCol<A>), A>,
    /// An impl: its self type's column and its trait reference's.
    pub impl_header: Option<(RegionCol<A>, Option<RegionCol<A>>)>,
    /// An opaque type: how many lifetimes it captures (edition rules and `use<..>` applied).
    pub opaque_captures: Option<u32>,
}
pub struct ClauseRegions<A: Allocator> { pub nth: u32, pub binders: u32, pub col: RegionCol<A> }
pub enum Outlives { Type { clause: u32, region: Region }, Region { long: Region, short: Region } }

/// What a reader asks (the flow lowering's `Facts` supplies it from head records).
pub trait RegionFacts {
    fn sig(&self, item: ItemKey) -> Result<SigRegions<'_>, RegionsUnread>;
    fn clauses(&self, item: ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread>;
    fn outlives(&self, item: ItemKey) -> Result<&[Outlives], RegionsUnread>;
    fn field(&self, adt: ItemKey, variant: u32, field: u32) -> Result<ColView<'_>, RegionsUnread>;
    fn impl_header(&self, impl_: ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread>;
    fn opaque_captures(&self, opaque: ItemKey) -> Result<u32, RegionsUnread>;
    fn lifetime_params(&self, item: ItemKey) -> Result<ParamsView<'_>, RegionsUnread>;
}
pub struct SigRegions<'a> { pub params: ParamsView<'a>, pub col: ColView<'a>, pub written_output: Option<ColView<'a>> }
/// Why a head's regions were not read: the item named, the cause typed (rule 19); never a guess.
pub struct RegionsUnread { pub item: ItemKey, pub why: UnreadWhy }
pub enum UnreadWhy { NoHead, HeadRefused, RecordForm { found: u32 } }
```

`ColView` and `ParamsView` are borrowed reads of a stored `RegionCol` and `LifetimeParams` (slots by
index, `own()`, `child(k)`; count, `is_late(i)`, outlives pairs): the same reads the owned types give.

At a call the flow maps each callee `Param(i)` to one fresh origin a call and each `Late` to one a
call a variable; type-parameter positions come from the call's erased argument types counted by
`region_arity` (the one counter; `npos` is it). A closure's signature regions come from the clause
that expected it: the checker records WHERE the expectation came from (`SigFrom::Clause { expr, nth }`,
`Written`, `Inferred`; `Object` and `FnPtr` refused by name at first), never a copy; `Late` slots are
universal origins of the closure body, the others are the OWNER's origins and their relations
become owner constraints. A by-reference capture's loan lives as long as the closure value
(`let read = || value; value = 1; read();` is E0506: unreported today, the first control of piece L5).

Pieces, written at once against the contract (one writer each, files disjoint):
- **L1a** the fn-signature producer: front2 types.rs (`RegionSink` beside each intern), heads.rs.
- **L1b** clauses, ADT fields, impl headers, opaque captures: front2 preds.rs, typeck world.rs
  (`PClause::TypeOutlives`, `RegionOutlives`), a new front2 heads_regions.rs.
- **L2** region_facts.rs and the head record, VERSION 22: front2 head_record.rs; restore equals fresh.
- **L3** the fn consumer: front2 borrows.rs, flow-lower lib.rs `signature()`, sig.rs reduced to views.
- **L4** the clause consumer: front2 call_generics.rs, borrows_ties.rs; flow-lower trait_ties.rs,
  generic_call.rs, param_outlives.rs.
- **L5** closures: typeck rules/closure.rs, infer/mod.rs `closure_hint`, columns.rs; flow-lower
  closure.rs; front2 borrows_nested.rs.
- **L6** (after the build) deletions: sig/{alias,impl_life,outlives,value}.rs, lifetimes.rs,
  borrow_sig_world.rs, sig_impl.rs, async_output.rs, and a ban on `read_signature`.
First controls, each piece: `fn f<'a, 'b>(y: &'b u8, x: &'a u8) -> &'a u8` called so that confusing
'a and 'b accepts a dangling borrow is E0597; `fn g<T: 'static>(t: T)` given `&local` is refused.
NOT of this family (other work): a future's auto traits from another fn's witness, hidden-type
questions, unnormalised projections (about 220 bodies), a mod in a macro-written block.

## For a successor

- Read BUILD-STATUS.md for the last lines of blaze-flow, blaze-flow-lower and the census; the
  census output is port27/flow-1116/flow-census-2.txt. Compare its head line with "findings 1323
  (in 314 bodies)" and its new line "signatures: ..", which says how many signature texts were
  not read and why: the largest refusal there is the next thing to teach `read_signature`, or
  the first thing H1 replaces.
- Invalidated in derived `fmt` and `eq` (131): print one flow (`Flow::print`) and confirm or
  drop the supposed cause in the table above before changing anything.
- A struct literal is related as a call of unknown signature: exact per field needs the ADT part
  of H1.
- The implied-bound fixture through the type checker needs a deref expression in the fixture
  body builder (`Bb`); only the hand-built flow covers it today.
- Refusals next: Closure 130 (captures as an `Aggregate` of borrows and moves, section 1), then
  whatever census 2 lists first.
- T1 and T2 (section 10) are unanswered.
- Tier one cannot accept a MUTABLE reborrow returned into a placeholder (`fn h<'a>(x: &'a mut
  (u8, u8)) -> &'a mut u8 { &mut x.0 }`): the loan is live everywhere, so its own issuing borrow
  looks like a write under it. Tier two accepts (fixture
  `signature_returns_a_reborrow_through_a_mutable_reference`). Every `&mut self -> &mut T`
  accessor goes to tier two, which costs 46 ms a body today: the dense tier two is the floor's
  problem, and this is its first common customer. Excusing a loan's own statement in tier one is
  NOT sound (in a loop the earlier iteration's loan is live there).
- The hole "an unknown callee cannot say `'static`" is CLOSED (leg 3): a call whose head is
  unknown, with an argument or result that carries a lifetime, is refused (`Form::CalleeUnknown`);
  a call through a value is related by the value's own signature, read from the written type of
  the body's parameter it is (`sig/value.rs`, `read_callable_param`: fn pointers, `dyn Fn`,
  `impl Fn`, `F: Fn(..)`; every region but `'static` fresh per call, `'static` the body's), else
  refused (`Form::CalleeValueUnknown`). Still to do there: the census's facts must answer
  `Facts::own_text`; a callable that names the enclosing fn's lifetime, or is a field or a
  local, is refused until T3 exists.
- STILL OPEN, the same hole in other clothes: (1) a struct literal or ctor whose field type
  writes `'static` (`struct K { r: &'static u8 }`, `K { r: &local }`) is related through one
  origin and accepted: needs the ADT part of H1, or a refusal until then; (2) a known head that
  writes no lifetime but whose bounds imply one (`T: Any`) called with generic arguments that
  carry origins keeps the one origin.
- The 131 "invalidated in a derived `fmt`": cause confirmed by putting it back
  (`Plant::UnknownUnified`, fixture `a_builder_chain_keeps_each_loan_in_its_result`): the one
  origin of an unknown-head call carried the receiver's loan into the receiver's own lifetime
  position. Such calls are now by signature (accepted) or refused; none is related that way.
- Closures: a closure expression is lowered as the borrows and moves of its captures
  (`closure_value`; its type's positions are its captures'), and a call of it borrows the
  closure value by its kind. Its BODY is not lowered, so the form stays refused unless the
  caller says it checks closure bodies (`Facts::closure_bodies_checked`, default false). NEXT:
  lower each closure body as its own flow (captured locals as inputs: by value like a
  parameter, by reference never dropped there), then answer `true` in the census. A closure
  call whose arguments or result carry a lifetime is refused (`CalleeValueUnknown`).
