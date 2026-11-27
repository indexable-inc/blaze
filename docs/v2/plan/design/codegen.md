---
status: plan
summary: The code builder's rules (crates/blaze-build), as built, with what is left to build - drop flags for a match by value on an enum that needs drop, trait objects (vtable layout, dynamic calls, what a key holds of a trait), closures and the rust-call convention.
date: 2026-10-29
---
# Codegen: the code builder's rules

How crates/blaze-build turns a checked body into LIR where the Reference leaves the layout to the
compiler. Each part is built; what is left is said at its end.

## A `match` by value on an enum that needs drop

The rule, from the Reference ("Match expressions", "Destructors", "Temporary scopes"):

- The scrutinee is a place expression. A local named there is matched in place, and is not
  moved. A value expression (`match f() { .. }`) gets a temporary, and the temporary's scope is
  the enclosing statement.
- A by-value binding of a field moves that field out of the place: a partial move. The `_`
  pattern, `..` and a unit variant move nothing. A binding of the whole scrutinee moves all of
  it.
- What is not moved out is dropped when the place's own scope ends, field by field, for the
  live variant only. For a local that is the local's scope end. For a temporary it is the
  statement's end.
- Bindings drop before the temporary, at the end of their arm. An enum with its own `Drop`
  allows no move out (E0509).

So after a `match x { .. }` on a local, what `x` still owns depends on which arm ran and what
that arm moved. That state lasts past the arms' join, to `x`'s scope end, and there is any code
in between.

**Built: drop flags per move path.** A local that some arm partially moves gets one 64-bit word,
one bit per field of all its variants:

- the bit is set at the `let` and at every assignment;
- a moving arm clears its fields' bits, with one `and`;
- every drop of `x` (its scope end, a `break`, `continue` or `return`, an assignment over it)
  switches on the discriminant and drops each field of the live variant whose bit is set.

A whole move uses the flag the binding already has.

A temporary is elaborated per arm, with no flag. The arm knows its variant and what it moved,
statically. The remainder joins the arm's drop scope ahead of its bindings, so it drops after
them at the arm's end, and on any early exit from the arm. The arm's end is the statement's end
when the `match` is a statement's whole expression, a `let`'s initializer, an arm's body, or a
block's tail in edition 2024 (where tail temporaries drop before the block's locals). Anywhere
else, a later operand would run before the temporary's drop, so that case is refused by name.

Why flags and not per-arm elaboration for a local: per-arm elaboration has to copy everything
after the match up to `x`'s scope end once per arm state. Sequential matches multiply those
copies; flags grow linearly. Flags are also the floor here:

- one store at the `let`;
- one `and` per moving arm;
- one load, `and` and branch per droppable field at the drop.

A pass that knows a flag's value at a drop (one arm, or no moves on any path) folds it to the
static form, which is the temporary's case. Static elaboration where the state is known and a
flag where it is not is the end state; only that fold is left to build.

## Trait objects: vtables and dynamic calls

**A wide pointer** (`&dyn Tr`, `*const dyn Tr`, `Box<dyn Tr>`, `Rc`, `Arc`) is two words: the
data address at 0 and the vtable address at 8. `dyn Tr` itself is unsized.

**The vtable** is an array of 8-byte words:

- word 0: `drop_in_place::<T>`, or 0 when `T` needs no drop;
- word 1: the size; word 2: the alignment;
- then the methods.

Method order is a function of the traits' text alone, so two builders agree. Walk the
supertraits depth first in their declaration order, the principal last. Each trait contributes
its methods in declaration order, and a trait reached twice contributes them only once. A
method with `where Self: Sized`, or with generics, gets no slot.

So the first supertrait chain is a prefix of the table, and upcasting along it is free: the
pointer stays the same. Every other supertrait `S` gets one word, right after `S`'s own methods,
holding the address of `(T, S)`'s vtable. Upcasting to `S` is one load.

The alternatives cost more. Pointers to every supertrait table make every inherited call two
loads. Copying every supertrait's whole table inline makes the code bigger and still needs the
pointers for a diamond.

Adding a method changes the vtables of that trait and the traits below it, nothing else. Auto
traits and `ExProj` bindings add no slot: `dyn Tr + Send` uses `dyn Tr`'s table.

**Identity.** One data record per `(T, principal trait with its arguments)` per program. It is
keyed by those two ground types, so builders share it by content across units. Its method words
are relocations to instance keys (`DataTarget::Fn`), never addresses. An edit to one impl method
therefore rebuilds that function alone. The vtable's bytes and key do not change, and the
patcher retargets the instance's entry.

The key also covers the principal's *shape*: the world's hash of the method lists and
supertraits the table reads (`vtable::ObjectTrait::shape`). Without it, an added method would
put two tables under one key (rule 21). A method appended to a trait changes the keys of that
trait's tables and of those below it. A method inserted in the middle also moves the slot of
every method after it, in those same tables. Built in crates/blaze-build/src/vtable.rs
(`Shape::of`, `Vtable::build`, `Vtable::record`, `vtable_key`), with tests in vtable/tests.rs.
Layout is in layout.rs (`Tail::Dyn`). `size_of_val` and `align_of_val` are in intrinsics.rs.

**Checks the builder stands in for.** The rebuilt path has no move checker or borrow checker.
Until one exists on the item layer, the builder's refusals are the only stop for E0509 (a move
out of a type that implements `Drop`, `a_match_by_value_refuses_drop_impl_enums_and_wide_enums`).

**A dynamic call** (the trait's method with `Self`, its first recorded argument, the `Dynamic`
type: the checker records no other mark) is one load
and one indirect call: `f = load [vtable + 8 * (3 + slot)]`, then `f(data, args..)`. The vtable
address is already in a register, as the wide pointer's second word. This is the floor.

For every dispatchable receiver (`&self`, `&mut self`, `Box<Self>`, `Rc<Self>`, `Arc<Self>`,
`Pin<P>` of these), the callee receives the data word: the receiver's thin form, which is
`DispatchFromDyn`'s rule.

**Unsizing** (`&T -> &dyn Tr`, raw pointers, and `Box`/`Rc`/`Arc` through `CoerceUnsized`)
pairs the thin pointer with the address of `(T, Tr)`'s vtable. That happens at the coercion the
checker records. `Box` waits until the checker records `CoerceUnsized`.

**Through the object:** `size_of_val` loads word 1 and `align_of_val` word 2. A drop calls word
0 on the data address when the word is not 0. A `Box` then frees with words 1 and 2.

**What a function's key holds of a trait** (built: blaze-build `object_reads`, `vtable::record_key`).
A dynamic call's code holds its slot, and an upcast off the first supertrait chain holds its word's
offset; both are functions of the principal's shape, which the world gives before the build, so the
key holds the finer fact, not the shape hash: (principal, method, slot) per dynamic call and (from,
to, word or "prefix") per upcast, in expression order. Adding a method at the end of a trait then rekeys no
caller of an existing method of that trait, only callers whose slot moved (a subtrait's own methods,
which follow the supertrait's words); the whole shape hash would rekey every caller. An unsizing to
`dyn Tr` holds nothing of the shape: its table is a relocation by (concrete, principal, args), which the
link resolves to today's vtable key. `size_of_val`, `align_of_val` and drop through an object read the
fixed words 0 to 2. The cost of the finer form is one pass over the checked body before the build.

## Closures and the Fn traits

Built in crates/blaze-build/src/closure.rs, a child module of walk.rs (tests in
`crates/blaze-build/src/tests/closures.rs`):

- ENVIRONMENT: a tuple with one field per capture in the checker's order: `&T` for `ByRef`,
  `&mut T` for `ByMutRef`, `T` for `ByValue`; zero-sized when nothing is captured; its drop glue
  is the tuple's.
- IDENTITY: the ground type is the checker's `Closure { owner, ordinal, args }`; kind, signature
  and environment are facts asked through `LayoutFacts::closure(owner, ordinal)` and grounded at
  `args` as an ADT's fields are. No expression id leaves one body's walk.
- BODY: the instance of the kind's own method (`Fn::call`, `FnMut::call_mut`,
  `FnOnce::call_once`) at `[C, (A..)]`, so a direct call `f(x)` and a call through the trait reach
  the same code. The weaker kinds' methods are thin entries over it: `call_once` on an
  `Fn`/`FnMut` closure takes the environment by value, calls the body with its address and drops
  it after. A fn item's or fn pointer's `Fn*` methods are shims.

blaze's "rust-call" convention (`Abi::RustCall`; core's `extern "rust-call" fn call_once(self,
args: Args)`): the last parameter, a tuple, crosses the call as its FIELDS, each a Rust-ABI
parameter of its own, after the others (`self` first); a zero-sized tuple is nothing. The callee
puts the tuple back in a slot of its own; a caller that holds the tuple whole spreads it field by
field. So a call through the trait and a direct call of the same body agree, and a closure's body
takes `(env, a, b)` with no repacking.
