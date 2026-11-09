---
status: plan
summary: Compile-time reflection in blaze. Phase 1 completes nightly `core::mem::type_info` in blaze's const evaluator; phase 2 is the opt-in `blaze_comptime` extension (`for const f in fields_of::<T>()`, `self.[f]`, `for<F: Field<T>>` bounds, `fn_info`), and 2b adds `dyn` and fn-pointer calls in const checked at evaluation and `const static { .. }` blocks that run any code at compile time; phase 3 builds types from const code (`type_of!`).
date: 2026-11-09
---
# Compile-time reflection (`comptime`)

Three phases, each usable on its own:

1. **`core::mem::type_info` complete.** The nightly's reflection API (`feature(type_info)`,
   rust-lang/rust#146922) answers in blaze's const evaluator as rustc's const eval does, and
   coretests' reflection tests pass under it. No extension: this is the pinned nightly's API.
2. **Generic code over fields and variants** (`#![feature(blaze_comptime)]`, a blaze
   extension). A loop unrolled once per field, whose body is type-checked once, up front,
   against bounds that quantify over the fields:

   ```rust
   #![feature(blaze_comptime)]
   use core::reflect::{Field, Struct, fields_of};

   fn hash_fields<T: Struct, H: Hasher>(value: &T, state: &mut H)
   where
       for<F: Field<T>> F::Type: Hash,
   {
       for const f in fields_of::<T>() {
           value.[f].hash(state);
       }
   }
   ```

   Function reflection sits beside it: `fn_info(&f)` gives const facts about a function,
   including its `code_id`.
   Phase 2b, under the same feature: `dyn` and function-pointer calls in const, checked when
   they are evaluated, and `const static { .. }` blocks, which run any code that can run at
   compile time (`format!(..).leak()` as a `&'static str`).
3. **Types built by const code** (`type_of!`). Design only; see the last section.

Rules this follows: superset-of-rust (phase 2 is opt-in and changes nothing for a crate that
does not opt in), semantics-in-front-end (everything below is front end: hir-def, hir-ty,
the solver, hir-ty-mir), errors-name-the-thing (a bound that fails names the field), and
deterministic-output (fields in declaration order, never layout or hash order).

## Phase 1: `core::mem::type_info` (done in this change)

`crates/hir-ty-mir/src/mir/eval/shim/reflection.rs` already answered every reflection
intrinsic of the nightly (`type_id_type_of`, `type_id_fields`, `type_id_variants`,
`type_id_generics`, `type_id_function_ptr`, `type_id_vtable`, the field-id family). What was
missing against the pinned library and coretests (`library/coretests/tests/mem/type_info.rs`,
`fn_ptr.rs`, `trait_info_of.rs`, `intrinsics.rs::test_type_id_vtable`):

| Gap | Fix |
| --- | --- |
| `#[rustc_splat]` on a fn pointer type's input was dropped: `FnPtr::splatted()` was always `None` (coretests `fn_ptr::test_splat`). Fn pointer inputs also ignored `#[cfg]` (RFC 2565 allows it there as on a function's). | `hir_def::hir::type_ref::FnType::splatted` (rustc's `FnDecl::splatted`: the first input with `#[rustc_splat]`, none on a C-variadic pointer), persisted (`ExpressionStore`/`Body` schema 9), lowered into `FnSigKind::set_splatted` (`hir-ty/src/lower.rs`), reflected as `is_splatted`/`splatted_index`. `#[cfg]` is checked per input. |
| A field's `TypeId` was a private table entry, not a type: `type_id_field_representing_type(..) != TypeId::of::<field_of!(..)>()`, and the id meant nothing at run time (`ComplexMemoryMap::type_ids` held `None`). | A field's id is the id of `FieldRepresentingType<Base, VARIANT, FIELD>`, as rustc's `Ty::new_field_representing_type` makes it (lang item `FieldRepresentingType` added to `hir-def/src/lang_item.rs`). The field-id intrinsics read it back with `AdtDef::field_representing_type_info`, rustc's rule. `TypeIdOf` is gone; the evaluator's id table is plain types. |
| `field_offset::<F>()` (what `core::field::Field::OFFSET` is) had no answer. | Answered from the FRT's base, variant and field (`Evaluator::field_offset`), as rustc's `field_offset` arm does. |
| A `DynMetadata` in a constant (`trait_info_of::<dyn Tr>().get_vtable()`, coretests `trait_info_of::test_dyn_creation`) kept const eval's vtable id as if it were an address: a wrong pointer at run time, silently. | `hir-ty-codegen/mod.rs` `relocate`: a `DynMetadata<dyn Tr>` gets the vtable of its type for `Tr`, as a `&dyn Tr` does. A `DynMetadata<*const ()>` (what `type_id_vtable` and `trait_info_of_trait_type_id` return) is a typed error naming the type, because the id says which type but not which trait. |

Tests: `crates/hir-ty-mir/src/tests/consteval/intrinsics.rs`
(`type_info_field_ids_are_field_representing_types`, `type_info_field_offset`,
`type_info_fn_ptr_splat`); the earlier `type_info_*` tests now go through FRTs.

Left, each a known gap rather than a quiet difference:

- **Vtable ids carry no trait.** `VTableMap` is keyed by type only, so a constant
  `Option<DynMetadata<*const ()>>` from `type_id_vtable` (coretests
  `intrinsics::test_type_id_vtable`, the second half of `trait_info_of::test_dyn_creation`)
  cannot be emitted, and stops with the error above. The fix is rustc's: a vtable id names
  `(type, dyn type)` (`GlobalAlloc::VTable(ty, dyn_ty)`). It is one port across
  `hir-ty/src/mir.rs` (`VTableMap`), the constant encoders (`hir-ty/src/consteval.rs`,
  `consteval/deps/codec.rs`), the identity hashes (`hir-ty-identity/src/facts.rs`,
  `blaze-items/src/{facts,form}.rs`, `hir-ty-pure`) and codegen; it waits for lanes identity
  and pure to land, which edit those files now.
- `ComplexMemoryMap::type_ids` is still `Vec<Option<Ty>>`; the evaluator makes no `None`
  now. Narrowing it to `Vec<Ty>` is the same kind of port, with the same files.
- `builtin # field_of(..)` (the `field_of!` macro, `feature(field_projections)`) is not parsed
  as a type, so a program can name an FRT only by `FieldRepresentingType<..>`. coretests'
  type_info tests do not use it.
- `field_offset` called at run time (not in const) reaches codegen, which has no arm for it
  and stops with its unsupported-intrinsic error. rustc answers it in codegen by const eval.
- `Type::of::<(A, B)>()` is refused, as rustc ICEs on it at the pinned nightly.

## Phase 2: `#![feature(blaze_comptime)]`

### Opt-in and where the API lives

A crate opts in with `#![feature(blaze_comptime)]`. The API is a module of blaze's `core`,
`core::reflect` (re-exported as `std::reflect`), every item
`#[unstable(feature = "blaze_comptime")]`, and the module is one of blaze's library patches
listed in `library/UPSTREAM`.

Why a crate feature and not `[workspace.metadata.blaze] experimental`: the extension adds
syntax and library items to one crate's language, and unstable library items are gated per
crate anyway. Under rustc, `#![feature(blaze_comptime)]` is an unknown feature (E0635) and
`core::reflect` does not exist, so such a program is refused, never miscompiled. A crate
without the attribute sees no new item and no new syntax: `for const`, `.[ ]` and bounded
`for<F: ..>` binders are refused at lowering, naming the feature.

Why `core` and not a blaze prelude crate: the traits below are answered by the solver, so they
are lang items, and lang items live in `core`; a prelude crate would have to be injected into
every crate graph and still could not hold them. The cost is one file
(`library/core/src/reflect.rs`) and one `pub mod` line carried through each nightly bump.

### The API

Written (lane comptime-library, 2026-11-02): `library/core/src/reflect.rs`, its lang items in
`crates/hir-def/src/lang_item.rs` (`Reflect*`) and blaze-diagnostics' lang-item table, its
symbols in `crates/intern`, and the `library/UPSTREAM` entry. The file is the reference; in
short (every item `#[unstable(feature = "blaze_comptime", issue = "none")]`, every trait
`#[rustc_deny_explicit_impl]`):

```rust
#[lang = "blaze_struct"] pub unsafe trait Struct { const NAME: &'static str = /* blaze_adt_name */; }
#[lang = "blaze_enum"]   pub unsafe trait Enum: Sized { const NAME: &'static str = /* .. */; }

#[lang = "blaze_field"]
pub const unsafe trait Field<Base: ?Sized>: crate::field::Field {
    const INDEX: usize = /* blaze_field_index::<Self>() */;
    const NAME: &'static str = /* field_representing_type_name(type_id::<Self>()) */;
    const ATTRS: &'static [&'static str] = /* blaze_field_attrs::<Self>() */;
    fn name(self) -> &'static str { Self::NAME }   // `"0"` for a tuple's
    fn index(self) -> usize { Self::INDEX }
    fn has_attr(self, name: &str) -> bool { /* name in Self::ATTRS */ }
}
#[lang = "blaze_marked"]   pub trait Marked<M: ?Sized>: crate::field::Field {}
#[lang = "blaze_unmarked"] pub trait Unmarked<M: ?Sized>: crate::field::Field {}

#[lang = "blaze_variant_representing_type"]          // the type `for const v` binds; ZST
pub struct VariantRepresentingType<T: ?Sized, const VARIANT: u32> { .. }
#[lang = "blaze_variant"]
pub const unsafe trait Variant<Base: Enum>: Copy + Send + Sync {
    #[lang = "blaze_variant_view"] type View: Struct;  // = VariantOf<Base, INDEX>
    const INDEX: usize = /* blaze_variant_index::<Self>() */;
    const NAME: &'static str = /* type_id_variant_name(type_id::<Base>(), INDEX) */;
    const DISCRIMINANT: <Base as DiscriminantKind>::Discriminant = /* blaze_variant_discriminant */;
    fn name(self) -> &'static str;  fn index(self) -> usize;
    fn fields(self) -> Fields<Self::View>;                      // fields_of::<Self::View>()
    fn of(self, value: &Base) -> Option<&Self::View>;          // discriminant compare, cast
    fn of_mut(self, value: &mut Base) -> Option<&mut Self::View>;
}
#[lang = "blaze_variant_of"] #[repr(transparent)]
pub struct VariantOf<T: ?Sized, const VARIANT: u32> { value: T }

#[lang = "blaze_fields"]   pub struct Fields<T: ?Sized, P = ()> { .. }  // P: the filter
#[lang = "blaze_only_marked"]   pub struct OnlyMarked<M: ?Sized, Rest = ()> { .. }
#[lang = "blaze_only_unmarked"] pub struct OnlyUnmarked<M: ?Sized, Rest = ()> { .. }
impl<T: ?Sized, P> Fields<T, P> {
    pub const fn marked<M: ?Sized>(self) -> Fields<T, OnlyMarked<M, P>>;
    pub const fn unmarked<M: ?Sized>(self) -> Fields<T, OnlyUnmarked<M, P>>;
}
#[lang = "blaze_variants"] pub struct Variants<T: ?Sized> { .. }
pub const fn fields_of<T: ?Sized + Struct>() -> Fields<T>;   // also V::View
pub const fn variants_of<T: Enum>() -> Variants<T>;

#[lang = "blaze_fn_item"] pub unsafe trait FnItem {}   // fn items and closures, not fn pointers
#[non_exhaustive] pub struct FnInfo {
    pub name: &'static str, pub path: &'static str, pub code_id: [u8; 32],
    pub inputs: &'static [TypeId], pub output: TypeId, pub is_const: bool, pub is_unsafe: bool,
}
impl FnInfo { pub const fn of<F: FnItem>() -> FnInfo { const { blaze_fn_info::<F>() } } }
pub const fn fn_info<F: FnItem>(f: &F) -> FnInfo;      // FnInfo::of::<F>()
```

Decided while writing it:

- **Constants carry the facts, methods read them.** Each fact is an associated constant whose
  default calls an intrinsic, as nightly's `core::field::Field::OFFSET` is
  `field_offset::<Self>()`; the methods return the constants. So every intrinsic is called only
  in a constant's initializer, never reaches codegen, and every method is usable in a
  `const { .. }` (the loop's `f` has a type nobody can name, so a body reads facts through
  `f.name()`, not `F::NAME`). `has_attr` searches `ATTRS` in const code, so it works at run
  time too.
- **The intrinsics** are private to `core::reflect` (`mod intrinsics`), all
  `#[rustc_intrinsic] #[rustc_comptime]`, answered by blaze's const evaluator only:
  `blaze_adt_name::<T>() -> &'static str`, `blaze_field_index::<F>() -> usize`,
  `blaze_field_attrs::<F>() -> &'static [&'static str]`, `blaze_variant_index::<V>() -> usize`,
  `blaze_variant_discriminant::<V, E>() -> <E as DiscriminantKind>::Discriminant`,
  `blaze_fn_info::<F>() -> FnInfo`. Field names reuse nightly's
  `field_representing_type_name`, variant names `type_id_variant_name`. The first five are in
  `hir_ty::intrinsic::Intrinsic` (2026-11-02) in a range of blaze's own after rustc's
  (`Intrinsic::FIRST_BLAZE`, append-only, since an intrinsic's index is persisted in MIR words
  and hashed into code identity), each classed as reading its arguments' identity, and the
  evaluator answers them (`eval/shim/reflection.rs`). `blaze_field_attrs` gives the empty slice
  for every field today: `#[blaze::attr(..)]` is not one of blaze's attributes yet, so no
  accepted program has one. `blaze_fn_info` waits for the fn_info lane; until then its use is
  the typed `UnknownIntrinsic` error at the constant that asks.
- **A variant is two types.** The token `VariantRepresentingType<E, v>` (zero-sized, `Copy`, what
  `for const v` binds and what `Variant<E>` holds for) and the view `VariantOf<E, v>`
  (`repr(transparent)` over `E`, what `of` returns and what the variant's fields belong to).
  One type cannot be both: the view holds an `E`, so it is not a zero-sized constant. So a
  variant's fields are `Field<V::View>`, and `View: Struct` is an item bound, which lets generic
  code call `fields_of::<V::View>()`. In a `for const v` body the variant's type has no name,
  so the body writes `v.fields()`.
- **`of` compares discriminants by bytes.** `DISCRIMINANT` is the variant's
  `discriminant_value`; `of` compares it with the value's byte by byte, because `PartialEq` of a
  discriminant type is not callable in a `const` trait's body. A discriminant is an integer, so
  every byte is initialized.
- **Filters are types, marks are traits.** `Marked<M>`/`Unmarked<M>` are the bounds; a loop's
  filter is the type parameter `P` of `Fields<T, P>`, a chain of `OnlyMarked<M, Rest>` and
  `OnlyUnmarked<M, Rest>` ending in `()`, since a trait cannot be a type argument and a struct
  cannot share a trait's name in one module. The solver and the expansion read the same chain.
- **`fn_info` takes `&F`**, so every closure qualifies, not only `Copy` ones; `FnItem` has no
  `Copy` supertrait. `FnInfo::of::<F>()` is the same for code that has only the type (a macro
  expansion). `FnInfo` is `#[non_exhaustive]`, so facts can be added.

```

Two traits are named `Field`: nightly's `core::field::Field` (no parameter; `Base`, `Type`,
`OFFSET`), which stays exactly as the nightly has it, and `core::reflect::Field<Base>`, its
subtrait with the container as a parameter, which is what makes `for<F: Field<T>>` read as
"for every field `F` of `T`". `F::Type` resolves through the supertrait. For an enum the
container is the variant view `VariantOf<T, v>`, not `T`, because a field of an enum exists
only in one variant; nightly's `Field::Base` of the same FRT stays `T`.

### Syntax

| Form | Meaning |
| --- | --- |
| `for const PAT in EXPR BLOCK` | `EXPR` has type `Fields<B>` or `Variants<B>`. The block is unrolled once per field (variant) in declaration order. `PAT` binds a *const* of a type `F` that is a fresh type in the body (`F: const Field<B>`), so the binding is usable inside `const { .. }` like a const parameter. `break`, `continue`, labels, `return` and `?` behave as in a loop whose iterations are the copies. |
| `EXPR.[EXPR]` | A place: the field `f` names of the place `EXPR` (auto-deref'd to `B`, with `f: F` and `F: Field<B>`). Its type is `<F as core::field::Field>::Type`. Reading, borrowing, assigning and moving (out of an owned base, as a named field) follow the named field's rules. |
| `if const { COND } { .. } else { .. }` | Existing syntax: an inline const as the condition. Inside a `for const` body it is evaluated per copy and the dead arm is pruned before codegen, so the dead arm's constants are never evaluated. Both arms are type-checked (once, generically). |
| `where for<F: Field<T>> F::Type: Hash + Eq` | "For every field `F` of `T`, its type is `Hash` and `Eq`." Splits into one clause per bound, sharing the binder, as `T: A + B` splits. |
| `where for<V: Variant<T>, F: Field<V::View>> F::Type: Debug` | Every field of every variant. |

None of these parse as valid Rust today (`for const` and `.[` are syntax errors; rustc refuses
bounds on `for<..>` binders), so accepting them changes no program the nightly builds. The
parser accepts them in every crate; lowering refuses them without the feature.

`x.[f]` mirrors C++26's splice `x.[:m:]` (P2996) and the `.` keeps it apart from indexing:
`x[i]` is a runtime index into one element type, `x.[f]` a compile-time field choice whose
type depends on `f`.

### Which fields: visibility

`T: Struct` holds for a struct or tuple when every field is `pub`, or when the struct is
written `#[blaze::reflect]`. `T: Enum` holds for every enum. Unions are neither (no field can
be read safely); `repr(packed)` structs are not `Struct` in phase 2 (a borrow of `x.[f]` could
be unaligned, which rustc refuses for named fields too); by-value reads of packed fields can
come later.

Why not "the fields visible where `fields_of` is called" (C++26's `access_context::current()`):
a C++ template is checked after instantiation, at one site. A Rust generic body is checked
once, in the crate that defines it, and used from many. `hash_fields` above is defined in a
library and called from the crate that owns `T`; scope-based visibility would give the library
only `T`'s `pub` fields, and making the answer depend on the caller's scope would make
`T: Struct` true in one crate and false in another, which the solver cannot express (its
answers are global) without a hidden generic parameter in every instance key.

Why not every field, unconditionally: field privacy guards invariants (`Vec`'s `len` and
`buf`), and `x.[f] = ..` would let safe code break them. `#[blaze::reflect]` is the author's
consent, the same consent `#[derive(..)]` gives today, scoped to the type and visible in its
source. A struct whose fields are all `pub` already lets anyone do what reflection does.

### Type checking once

The body of `for const f in fields_of::<B>()` is inferred once, with `F` a placeholder type in
a fresh universe and the loop's own assumptions added to the body's param env:
`F: const Field<B>`, `F: Copy`. Every obligation registered in the body carries that env.
Goals about `F` are then solved from:

- the loop's assumptions (`F::Type` is rigid: nothing is known about it but its bounds);
- the function's field-binder clauses, `for<G: Field<B>> P(G)`, instantiated at `G := F`. The
  binder's own bounds (`F: Field<B>`) become nested goals, which the loop's assumptions prove.

So a body that calls `.hash()` on `x.[f]` without `for<F: Field<T>> F::Type: Hash` in scope is
an error at the call in the body, once, for every `T`, never at an instantiation. The only
per-instance failures are those Rust already has: a panicking `const { .. }` evaluated per
copy, and a recursion limit.

### The solver: field-binder clauses

Representation (as built, 2026-11-02): a field-binder clause is an ordinary
`Binder<ClauseKind>` whose bound variable is `BoundVariableKind::Reflected(ReflectedVar { kind,
domain, container })` (`vendor/rustc_type_ir/src/reflect.rs`): `domain` is `Fields` or
`Variants`, `container` the `T` of `Field<T>`, written inside the binder. The binder's bound is
the variable itself, not a clause beside it, so a binder cannot lose its range: folding,
visiting, flags and the persisted form (`next_solver/persist.rs`, bound variable tag 3) all
carry the container. No new `ClauseKind`. Lowering builds one with
`hir_ty::comptime::field_binder_clause` (lane infer owns `lower.rs`).

Two rules, in `vendor/rustc_next_trait_solver/src/solve/reflect.rs`:

1. **Using a clause.** A field binder in the environment never matches a goal directly
   (`assemble_param_env_candidates` skips it): an inference variable for `F` would stand for any
   type, not only a member. It reaches the goals through members: when a member `!F` of `C`
   enters the environment (rule 2's placeholder), the environment gains `!F: Field<C>`, what that
   elaborates to, and every field binder over the same domain and container instantiated with
   `!F` (`env_with_reflected_member`).
2. **Proving a clause** (`compute_goal`, before `enter_forall`): normalize the container, then
   - an inference variable: ambiguous;
   - a parameter, placeholder or rigid alias: prove `P(!F)` for a placeholder `!F` in a new
     universe, in the environment rule 1 extends. This is what lets one generic function call
     another with the same bound;
   - otherwise: `C: Struct` (`Variants`: `C: Enum`), then `P(m)` for each member `m` of
     `hir_ty::comptime::reflect_members_checked` (`FieldRepresentingType<C, 0, i>`; an enum's
     variants `VariantRepresentingType<E, v>`), in declaration order.

   A failure names the member: `solver_errors.rs` proves the members again in a probe and
   reports the first that fails as `SolverDiagnosticKind::ReflectedMemberUnmet` with the error
   for it inside, which blaze-diagnostics prints as "field `b` of `S` has type `NotHash`, which
   does not implement `Hash`", the inner error as a note. Not yet: the primary span at the
   field's declaration (the span is the obligation's).

`Struct`, `Enum`, `reflect::Field<B>`, `Variant<B>` and `FnItem` are builtin candidates
(`consider_builtin_reflect_candidate` in the trait, projection and host-effect goal kinds) next
to the existing FRT `Field` candidate; `<VariantRepresentingType<E, v> as Variant<E>>::View`
normalizes to `VariantOf<E, v>`. Known gaps: `core::field::Field` is still refused for an enum's
FRTs; a container naming the binder's own lifetimes gets no solution; GAT bounds
(`MapAndCompressBoundVars`) drop the reflected variable; the filters below are not built.

### Field filters (designed now, built after the plain form)

The binder's bound list narrows the domain: `for<F: Field<T> + Unmarked<Skip>> F::Type: Hash`
ranges over the fields of `T` not marked `#[blaze::mark(Skip)]`. `Marked<M>` and
`Unmarked<M>` are builtin traits on FRTs; `M` is any type (`struct Skip;`), so a mark is a
resolved path: a typo is an unresolved-name error, never a field quietly included, and marks
are namespaced by crate. The loop narrows the same way:
`for const f in fields_of::<T>().unmarked::<Skip>()`, typed `Fields<T, OnlyUnmarked<Skip>>`; the
loop's placeholder then assumes `F: Field<T> + Unmarked<Skip>`, so the filtered bound's nested
goals hold, and the expansion takes exactly the fields the same goals accept. Both sides are
decided by the solver from one predicate list, so they cannot disagree.

Composition: filters are a conjunction of bounds (`+`); a field is in the domain when all
hold. Rule 2 above expands over the filtered domain; rule 1 adds every bound as a nested goal.
Strings (`f.has_attr("skip")`) stay for `if const` pruning inside a body, where they decide
per copy; they never narrow a bound, because a string is not a path and cannot be checked.

### MIR: one generic body, unrolled at monomorphization

Borrow checking and every MIR pass run on generic MIR, before monomorphization
(`crates/hir-ty/src/providers.rs`: Built, Runtime, Optimized, then `monomorphize`). So the loop
stays one construct until an instance is made:

- `TerminatorKind::ComptimeFor { domain, binder, body, after }` starts the loop and
  `TerminatorKind::ComptimeNext { loop_ }` ends each iteration. Passes see an ordinary loop
  (successors `body` and `after`), which is what borrowck and dataflow need: the body may run
  any number of times. The MIR inliner does not inline a body that contains one.
- `ProjectionElem::ReflectedField(F)` is `x.[f]` in generic MIR. Borrowck treats it as it
  treats `Index`: it overlaps every field of the base, and two uses with the same binder in
  one iteration are the same place (`&mut x.[f]` twice conflicts).
- Moving out of `x.[f]` is refused for now (E0507, naming the place and the feature), behind a
  reference as rustc refuses a named field's, and out of an owned base too. Two things are
  missing for the owned case. First, rustc allows a move out of a field only from a base with
  no `Drop` impl (E0509), and a generic `B: Struct` may have one, so the move would be refused
  per instance, which breaks checking once; it needs a concrete base or a bound saying "no
  `Drop`". Second, drop elaboration runs on the generic body, where the open drop of the base
  after the move cannot say which field it skips. The plan: borrowck gives each binder one
  move path, reset on the `ComptimeNext` back edge, plus a "some field of `x` moved through
  `U`" state that conflicts with every use of `x` except `x.[f]` of that binder; drop
  elaboration leaves an open drop with a reflected child to a run after the expansion.
- Every pass after inference that solves goals over the body's types (borrowck, the MIR
  passes, the evaluator, unsafeck, the lints) solves them in the env inference used, with
  `InferenceResult::comptime_param_env` and an inference context raised to the binders'
  universes (`hir_ty::comptime_env`), so it gives the same answers inference got inside the
  loop.
- monomorphization (`crates/hir-ty-mir/src/mir/monomorphization.rs`, reached through
  `providers::Mir::monomorphize`) expands each `ComptimeFor` once its domain's base is
  concrete: the region is the blocks dominated by `body`; for field `i` it copies the region,
  gives every local whose type mentions `F` its own copy, substitutes `F := FRT<B, v, i>`,
  turns `ReflectedField` into `Field(i)` (`Downcast(v), Field(i)` on a `VariantOf`), and
  chains copy `i`'s `ComptimeNext` to copy `i + 1` (the last to `after`). Then a
  `SwitchInt` on a now-monomorphic inline const is folded and unreachable blocks are dropped,
  which is the `if const` guarantee (`monomorphization/comptime.rs` `fold_if_const`, run after
  the fill on a body that had a loop: the switch's operand is the constant or the block's last
  `_n = const C`; the blocks only the dead arm reached lose their statements and end in
  `Unreachable`). An inline const inside a loop is lowered with the body's identity arguments
  followed by one type argument per enclosing binder (`inline_const_args`), its own body reads
  the loop's pattern as the binder's ZST value, and the instance puts `args[n + k]` in place of
  binder `k` (`bind_inline_const`). A body with a loop has no shared compile identity: it is
  compiled per instance (`Unshared::PerInstance`), keyed by its arguments, whose identity holds
  each field's and variant's name (blaze-items form 3) besides their types and attributes. Nested loops expand outer first, since an inner domain
  can name `F`. Copies are numbered in declaration order (deterministic-output).

The construct lives in `crates/hir-ty-mir`; the domain rule (which fields, which filters,
`Struct`/`Enum`) in a new front-end crate `crates/hir-ty-comptime`, which the solver's
builtin candidates and the expansion both call, so the two cannot disagree.

### Enums

```rust
fn debug_enum<T: Enum>(value: &T, f: &mut Formatter<'_>) -> fmt::Result
where for<V: Variant<T>, F: Field<V::View>> F::Type: Debug,
{
    for const v in variants_of::<T>() {
        if let Some(x) = v.of(value) {
            let mut t = f.debug_tuple(v.name());
            for const g in v.fields() { t.field(&x.[g]); }
            return t.finish();
        }
    }
    unreachable!()  // an inhabited enum matched one variant
}
```

`v.of` compares the discriminant and casts `&T` to `&VariantOf<T, v>` (same layout,
`repr(transparent)`); `.[g]` on a `VariantOf` is a downcast then a field, both ordinary MIR
places, so borrowck needs nothing new. Building enum values (`VariantOf::new`) and
exhaustiveness without the `unreachable!()` are later work.

### Function reflection: `fn_info`

`fn_info(&f).code_id` is the one user-facing way to get a function's code hash: the behaviour
identity of lane identity's `hir_ty_identity::code_id` (that lane's handoff
`/tmp/blaze-lanes/v2-identity.md`; its plan doc, `docs/v2/plan/design/code-identity.md`, is
being written), the root instance being `f` with its generic arguments (`fn_info(&g::<u8>)`, or
a closure). `F: FnItem` holds for fn item and closure types only: a fn pointer has no code
known at compile time.

- **Answered by const eval, not codegen.** The library body (`FnInfo::of::<F>()`, which
  `fn_info(&f)` calls) is `const { intrinsics::blaze_fn_info::<F>() }`, an inline const, so the answer is a constant in
  the caller's MIR in every context, runtime or const, and never reaches codegen as a call.
  This matters for identity: `code_id` covers a body's constants' values but names a plain
  callee only by item key, so an intrinsic answered in codegen would give a caller the same
  CodeId before and after `f`'s code changes while its output changed (keys-cover-every-input).
  As a constant, `f`'s id is in the caller's id. Lane identity's planned
  `blaze_code_id::<F>()`, answered in hir-ty-codegen, becomes this intrinsic; the kernel reads
  `fn_info(&step).code_id`. One door.
- **Wiring.** `hir-ty-mir`'s evaluator cannot call `hir-ty-identity` (which depends on it), so
  `code_id` is reached through a table in `hir_ty::providers` that `hir-ty-identity` registers
  with `inventory::submit!`, as hir-ty-mir registers `providers::Mir`. A binary that links no
  table answers with a typed `MirEvalError` naming `f` and the crate to link (a job failure,
  not the panic `Mir` gives), never a zero id.
- `Unidentified` (a `dyn` call the walk cannot follow, a body that does not lower) is an error
  at the constant that asked, naming `f` and the chain identity reports.
- A function whose own constants ask for its own id (directly, or `f` asks for `g`'s and `g`
  for `f`'s) depends on itself. The query cycle is a typed error naming both functions and the
  constant, never a fixpoint and never a salsa panic.
- Under rustc: no `core::reflect`, so refused at compile time, never faked.
- Wired once `hir-ty-identity` compiles; the rest of `FnInfo` (name, path, inputs, output,
  const and unsafe) needs only what `type_id_function_ptr` already reads.

### Tests phase 2 must land with

- `Hash` and `Debug` written with `for const` over a 3-field struct of different types
  (`u8`, `String`, `Option<(i32, bool)>`) give the same output as `#[derive(Hash)]` (bytes fed
  to a recording `Hasher`) and `#[derive(Debug)]` (`{:?}` and `{:#?}`). `Debug` uses
  `T::NAME` with `debug_struct` and `field(f.name(), &x.[f])`, which is what the derive's
  `debug_struct_fieldN_finish` prints.
- A struct with a non-`Hash` field passed to `hash_fields` is refused, the error naming the
  field, its type and the bound, at the call, never post-mono.
- A crate without `#![feature(blaze_comptime)]` is unchanged: its MIR, the store keys of its
  bodies and its output are byte-identical with the extension compiled in, and each new form
  in it is a lowering error naming the feature.
- The body checked once: a body using `x.[f].to_string()` under a `Hash`-only bound is
  refused at the call in the body, also when `T` is never instantiated.
- Borrowck: `&mut x.[f]` twice in one iteration is refused; moving `x.[f]` out of `&T` is
  refused, naming the place.
- `if const` pruning: a copy whose dead arm holds `const { panic!() }` builds.

### Work split (lanes for phase 2)

Interfaces are this document's; each lane builds and lands on its own.

1. **syntax**: `crates/parser` (`for const`, postfix `.[ ]`, bounded `for<..>` binders in
   where clauses), the AST accessors (`ForExpr::const_token`, an index expression with a dot
   token), hir-def lowering to `Expr::ComptimeFor` and `Expr::ReflectedField`, the binder
   parameters' bounds in `GenericParams`, persistence (`hir/columns.rs`, `persist.rs`), and the
   feature check. Every exhaustive match over `Expr` updated in the same batch (rule 21).
2. **library** (written 2026-11-02, see "The API"): `library/core/src/reflect.rs`, lang items
   in `hir-def/src/lang_item.rs`, `library/UPSTREAM` entry. Left for the solver, mir and
   fn_info lanes: the six `blaze_*` intrinsics in `hir_ty::intrinsic::Intrinsic` and their
   evaluator arms (five written 2026-11-02 by the mir lane; `blaze_fn_info` left).
3. **solver**: builtin candidates and the two field-binder rules, errors naming the field
   (`hir-ty/src/solver_errors.rs`), `crates/hir-ty-comptime` (the domain).
4. **infer**: the loop's placeholder and param env, `.[ ]` place typing, const binding.
5. **mir**: the terminators and projection, borrowck rules, generic-pass handling, the
   expansion and the `if const` fold in monomorphization; the tests above. Written
   2026-11-02 except the passes' env and partial moves; of the tests, the `Debug` parity (a
   reduced fixture core: field names, indices and values, no `fmt`) and the `if const` one are
   written, not yet run.
6. **fn_info**: after lane identity lands.

## Phase 2b: evaluation in place of const checking

Two more pieces of `#![feature(blaze_comptime)]` (2026-11-09), for const code the nightly's
const checker refuses although the evaluator can run it: calls through `dyn` and function
pointers in a const context, checked when they are evaluated; and `const static { .. }` blocks,
in which nothing is const-checked and everything that can happen at compile time runs:

```rust
#![feature(blaze_comptime)]
const SYM: &str = "spin";
const PAUSE: &str = const static { format!(".globl {SYM}\n{SYM}:\npause\nret\n").leak() };
```

### `dyn` and fn-pointer calls in const

Without the feature, as rustc: the const checker (`hir-ty-mir/src/mir/const_check/calls.rs`,
rustc's `check_consts`) refuses a call through a function pointer (`FnCallIndirect`) and a
method call on `dyn Trait` of a trait that is not const (`NonConstFn`); `effects::non_const_calls`
refuses the same. With it, in a crate that has it, neither is a static error: the call is
checked where its target is known, when it is evaluated.

- **Which calls.** A `TerminatorKind::Call` whose callee is a function pointer, and a call the
  evaluator makes through a vtable: `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` whose
  `Self`, behind references, is `dyn Trait`, or another trait's method whose `Self` is a `dyn`
  type with the trait among its principal's supertraits (`is_dyn_method`). A method of an impl
  written for a `dyn` type, or for a reference to one, is a direct call the evaluator does not
  check, so it stays a static error. `effects::defers_dispatch_check` (the crate has the feature) and
  `effects::is_dyn_dispatch` are the one rule both checkers read.
- **The target.** The evaluator already runs these calls: a function pointer's bytes are a
  `VTableMap` id of the `FnDef` or closure it was made from, a vtable's id names the concrete
  type, and the method is called with `Self` that type. Before the call, it asks
  `effects::const_call_error(db, env, Const, callee, args)` of the target, the rule the static
  checker applies to a direct call: a `const fn` whose `[const]` bounds hold const, or a method
  of a `const trait` whose `Self: [const] Trait` holds (a `const impl`; a default method
  only through one). A closure is const when it is written `const ||`.
- **The error.** Otherwise `MirEvalError::Comptime(ComptimeError { kind: NonConstTarget { .. },
  at })`, naming the function that would run (a trait method resolved to its impl's), how it
  was reached (a function pointer, or the vtable of the `dyn` type), whether its `[const]`
  bounds were what failed, and the call site; the frames it propagates through
  (`MirEvalError::InFunction`) are the call chain.
- **When the check runs.** In an evaluation at compile time (a constant, a static, an anonymous
  constant), outside a `const static` block, at a call in a body that is a const context
  (`effects::body_constness`, or an anonymous constant) of a crate with the feature
  (`effects::dispatch_checked_at_eval`). That is exactly the set of calls the static check
  let through: a body of a crate without the feature never reaches one (refused statically),
  and a body that is no const context only runs at compile time inside a `const static` block.
  So std's bodies, which never have the feature, keep the evaluator's behavior, and the
  evaluator's own uses of function pointers (`fmt::rt::Argument` in a panic message) are not
  checked.

Why at evaluation, not a `dyn const Trait` type: rustc's `dyn [const] Trait` is not settled at
the pinned nightly and needs a vtable's constness in the type system. Checking the target
needs nothing of the type system, accepts every program whose evaluation reaches only const
code, is what rustc's own const eval does to a call it reaches ("calling non-const function"),
and names the concrete function, which a type error could not. A type-level guarantee for
generic code can come later on top.

### `const static { .. }` blocks

- **Syntax.** `const static { stmts }`, an expression. Not `comptime { .. }`: `comptime { x }`
  is valid Rust today (a struct literal of a type named `comptime`; `if comptime { .. }` with a
  `bool` of that name), so taking it would change programs that do not opt in, against the
  rule of this document's syntax that none of its forms parse as Rust. `const static {` is a
  syntax error under rustc (`const static` starts only a closure, `const static || ..`), adds
  no keyword and no `SyntaxKind` (whose numbers are persisted in source maps), and reads as
  `const { .. }` evaluated statically, as D's `static if` and C++'s `static_assert` use the
  word. The parser takes it in every crate as a `BLOCK_EXPR` with a `const` and a `static`
  token (`BlockExpr::is_comptime`); hir-def lowering refuses it without the feature
  (`ComptimeForm::Block`, the `ComptimeWithoutFeature` error) and with it lowers it as an
  inline const, `Expr::Const`: the same type checking, the same generic handling (evaluated per
  instance), no new `Expr` and no change to the store's format. Whether an `Expr::Const` is one
  is read from its syntax (`effects::const_block_is_comptime`, as `closure_is_const` reads a
  closure's `const`), after the crate's feature, so a crate without it pays one feature lookup.
  The formatter refuses it, as the other forms, rather than print a plain `const { }`.
- **Its own evaluation.** A `const static` block is always a constant of its own
  (`AnonConstId`, `inline_const_at`), also in a constant's or static's value, where `const { }`
  is lowered in place; and an evaluation that meets it evaluates it as an anonymous constant
  (`anon_const_eval`), not in its own run as it runs a `const { }`. So comptime is a property of
  one evaluation (`Evaluator::comptime`, from its owner), never of a frame, and nothing it
  allows leaks into the code around it.
- **No const checking.** `anon_const_check` reports nothing of such a block, and the dispatch
  check above is off in it: any function may be called (non-const functions, `dyn`, function
  pointers, std's `fmt` machinery, `String` and `Vec`).
- **What is refused**, each a `ComptimeError` naming the call or the site:
  - `Foreign { func }`: a call of an `extern "C"` function other than the memory ones the
    evaluator answers purely (`memcmp`, `memcpy`, `memmove`, `memset`, `strlen`): I/O
    (`write`, `open`), time (`clock_gettime`), randomness (`getrandom`, `CCRandomGenerateBytes`,
    so `HashMap::new()`'s `RandomState`), threads (`pthread_create`), the environment (`getenv`)
    and system calls. LLVM intrinsics and the allocator's shims are not foreign calls.
  - `PointerAddress { how }`: a pointer's address read as an integer, by an `as` cast
    (`PointerExposeAddress`, `expose_provenance`) or a `transmute` of a value with a pointer in it
    to a type with none (`ptr.addr()`). The evaluator's addresses are its own; one in the
    constant would differ from the program's and between builds.
  - Nondeterminism needs no rule of its own: the evaluator's only sources of it (a clock, a
    random state, scheduling) are reached through foreign calls, which are refused.
- **The heap.** What the program's allocator (`__rust_alloc`: `Global`, `Box`, `Vec`, `String`)
  allocates in the block is the block's const heap. At the end, a heap block the value reaches
  through a shared reference to `Freeze` data (or through a raw pointer in such a block) is
  made global: copied into the constant and immutable, as `const_make_global` makes a
  `const_allocate` block. So `format!(..).leak()` as a `&'static str` works. A block the value
  owns (a `Box`, `Vec` or `String` by value, one behind `&mut` or behind an `UnsafeCell`) is
  `HeapInValue`, as today: each use of the constant would copy its owner, and a drop would free
  the constant's memory. Heap the value does not reach is dropped, as in any constant.

### Without the feature

A crate without `#![feature(blaze_comptime)]`: `const_check` and `non_const_calls` report what
they did (both refusals above); `const static { .. }` is the lowering error naming the feature;
the evaluator never checks a dispatch target in its bodies and never evaluates in comptime
mode; MIR, store keys and output are unchanged. Keys: a `const static` block's value depends
on its code and the values it reads, as any constant's (keys-cover-every-input); every input
from outside the program (files, the environment, the clock) is refused, not read.

Cost: one cached feature lookup per inline const of a constant's body at MIR lowering and per
`anon_const_check`, the syntax read only in a crate with the feature; on a function-pointer or
`dyn` call at compile time, the feature lookup and, in a crate with it, one `const_call_error`.

### Tests

`crates/hir-ty-mir/src/tests/consteval/comptime.rs` (evaluation) and
`crates/hir-ty-mir/tests/mir/const_check.rs` (the static check):

- a `dyn` call of a `const impl` method in a `const fn` evaluates; one of a plain impl is the
  `NonConstTarget` error naming the impl's method and the `dyn` type;
- a function-pointer call of a `const fn` evaluates; of a plain `fn`, the error naming it;
- in a `const static` block, a `format!`-shaped run (a growing `String` over `__rust_alloc`,
  `dyn Write`, function-pointer formatters, non-const functions) ending in `leak()` gives the
  text as a `&'static str`; keeping the `String` itself is `HeapInValue`;
- a `const static` block calling `write` is `Foreign` naming `write`; one casting a pointer to
  `usize` is `PointerAddress`;
- without the feature: both static errors as before, the evaluator runs the same calls as
  before, and `const static { }` is the lowering error.

### Known gaps

- A pointer's bytes read as an integer through memory (`*(p as *const *const u8 as *const
  usize)`) are not caught: the provenance machine knows the bytes hold a pointer, but a load does
  not say which type it reads them as. rustc refuses it.
- A `dyn` call reached through a generic impl of std (`<Box<dyn Fn()> as Fn>::call`) is still
  refused statically, as std's impl is not const; through the evaluator it would be checked.
- `const_eval_select` takes its const arm in every evaluation, so a `const static` block runs
  std's const-eval paths where std has them (`align_offset` gives `usize::MAX`).

## Phase 3: types from const code (`type_of!`), design only

Goal: const code computes a type. `type_of!(EXPR)` is a type position whose `EXPR` is a const
expression of type `TypeId` (the inverse of `TypeId::of`) or of a description,
`core::reflect::TypeDesc`:

```rust
const fn with_ids(fields: &[FieldDesc]) -> TypeDesc { .. }      // ordinary const code
type Row = type_of!(TypeDesc::Struct(StructDesc { name: "Row", fields: &[
    FieldDesc { name: "id", ty: TypeId::of::<u64>(), attrs: &[] },
    FieldDesc { name: "name", ty: TypeId::of::<String>(), attrs: &[] },
]}));
```

- **Identity.** A described type is structural: interned by a content hash of its evaluated
  description (names, field types by identity, attributes, repr), so two equal descriptions
  are one type in every crate. The description's hash is the type's key
  (keys-cover-every-input), and its `type_name` is the description's name with that hash's
  prefix, deterministic across machines.
- **Back to a type.** `type_of!(id)` of a `TypeId` resolves through the evaluator's id table,
  which already maps every id it made back to its type (phase 1 made field ids real types for
  this reason).
- **Generic.** In a generic context `type_of!(make::<T>())` is an alias the solver normalizes
  once its arguments are concrete, rigid before that. What generic code may assume about it
  comes from a declared bound on the alias (`type Out<T> = type_of!(make::<T>()) where
  Out<T>: Debug;`), checked when the alias is normalized for concrete `T`. That check is
  per instance; it reports at the instantiating use, naming the alias and the bound, which is
  the one place phase 3 cannot avoid an instance-time error, so it is kept to declared bounds
  and never reaches into bodies.
- **Impls.** Auto traits follow fields; other traits come from phase 2's blanket code
  (`for const` over a described struct is the same loop), or from an `impls` list in the
  description naming impls generic over `T: Struct`.
- **Cycles.** Lowering a type calls const eval, which may lower types: a type whose
  description depends on itself is a typed cycle error naming the alias and the constant.
- **Why not types as values (Zig's `comptime`).** Zig checks a function body per
  instantiation, so a wrong use is reported at a caller, after the fact. blaze keeps Rust's
  check-once rule: const code computes descriptions, and a type exists only after
  `type_of!` has evaluated one.

## Prior art

- C++26 reflection (P2996) and expansion statements (P1306, `template for`): the same unroll
  per member with a per-iteration constant, and the splice `v.[:m:]` this design's `x.[f]`
  follows. C++ checks the body per instantiation and chooses members by an access context at
  the expansion site; blaze checks once against bounds and chooses by the type's consent
  (see "Which fields").
- Rust nightly: `field_projections` (field representing types, `core::field::Field`) is the
  type-level field this design ranges over; `type_info` (phase 1) is its value-level twin.
- Crates: `facet` (facet-rs/facet, derive-generated runtime shapes), `bevy_reflect` (runtime
  reflection through a derive), `bronzite` (reflection inside proc macros). Each needs a derive
  or proc macro on every type and works on runtime data or tokens; `for const` works on
  resolved types, with no derive, and costs nothing at run time.
- Zig `comptime` (types as values, checked per instantiation): see phase 3.
