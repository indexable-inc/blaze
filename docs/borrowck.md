---
status: current
summary: blaze's own borrow checker (hir-ty-borrowck): how it matches rustc's verdicts, how it is scored, what is left.
---
# The borrow checker

blaze has its own borrow checker, the crate `hir-ty-borrowck` in the front end
(`crates/hir-ty-borrowck`, next to `hir-ty`). It checks the MIR rust-analyzer
lowers, the same MIR the code generator compiles, and it is meant to give exactly rustc's
verdict: the same errors, on the same lines, and none where rustc gives none. It gives
rustc's verdict on 95% of rustc's own borrow-check tests today, and on every one rustc
accepts; how they are scored and what is left is below.

It runs as part of rust-analyzer's diagnostics, so the daemon computes it for every edited
file, and `blaze-engine borrowck` prints what it finds. The daemon refuses an edit it finds a
borrow-check error in (below), but the checker still misses about a sixth of the errors
rustc gives, so `cargo check` stays the final word.

## Refusing builds

An edit with a borrow-check error fails its build, with the file, line and error, as an edit
with a type error does; under `blaze`, cargo then runs and rustc reports it.
`BLAZE_BORROWCK_REFUSE=0` in the daemon's environment turns this off.

This is safe because the checker reports nothing on any test rustc accepts, nor on ripgrep,
fd, bat, hyperfine, just, axum, nushell, helix or biome: a refused build is one rustc refuses too. A
borrow-check error blaze reports where rustc reports none is a bug; `BLAZE_BORROWCK_REFUSE=0`
works around it until it is fixed. What it does not do: it misses about a sixth of the
errors rustc gives (it accepts code rustc rejects, listed below), so code with such an error
still builds and runs from memory.

## Modes

`BLAZE_BORROWCK` picks one:

- `polonius`, the default: rustc's Polonius alpha, which is what nightly rustc does by
  default (`-Zpolonius=next`). It accepts more programs than NLL: a loan ends where nothing
  that can still reach it is live, which accepts the "conditional return of a borrow" cases
  NLL rejects.
- `nll`: stable rustc's checker. The only difference from `polonius` is when a loan goes out
  of scope.
- `off`.

## What rustc rules it follows

All of these are rustc's, written from `compiler/rustc_borrowck` at the pinned nightly
([research/borrowck.md](research/borrowck.md) has the sources):

- Moves and initialization: moves out of places, partial moves, maybe-uninitialized reads,
  assignment to immutable locals (E0381, E0382, E0384, E0505, E0507, E0508, E0509). A `match`
  whose arms test the value reads what it matches first, so a value moved out whole is
  reported there. A `match` arm's guard sees the variables its pattern binds by reference, so
  it cannot move out of them (E0507), borrow them mutably (E0596) nor assign them or through
  them (E0594, "immutable for the pattern guard"), and casting an enum to an integer moves the enum
  (`*e as usize` behind a reference is E0507). A pattern's moves out of what it may not move
  out of are one error at the value matched for each place they move out of, as rustc's
  `MovesFromPlace`: `(D(s), &t)` is an E0509 and an E0507. A move of a subslice of an array
  moves each of its elements, so `let [_y @ .., _, _] = a` after `let [_x, _, _] = a` uses a
  moved element, and so does assigning `a[2]` after moving it out, as rustc reads the array
  to index it. rustc buffers the E0382s of a function and the closures in it in one map,
  keyed by the indices of the moves each is about, though each body numbers its moves from 0
  (`BorrowckDiagnosticsBuffer::buffered_move_errors`). A closure is checked first, so a use
  in the function whose moves have the numbers of an error the closure gave, of a place that
  is a prefix of the closure's (the variable `_1` and the capture `_1.0`), is not reported,
  and one of another place replaces the closure's: `drop(x); spawn(move || { drop(x);
  drop(x); })` is one error, in the closure. blaze numbers each move as rustc's MIR would,
  leaving out the moves its lowering adds (a capture copied into a local of the closure, an
  operand moved into a temporary first, a drop), and names a capture as the closure's field.
- Match guards (E0510): rustc fake-borrows, through every guard of a `match`, what its arms
  test and the references that and what they bind are behind, so a guard that assigns or
  mutably borrows one of them, closures included, could change which arm matches; assigning
  what the guard's arm binds by value is E0506, as the guard sees it through a reference.
  Indexing (E0510) too: in `x[1][{ x = y; 2 }]` with `x: &[&[i32]]`, the slice `x[1]`
  indexes was bounds-checked before the second index, so what it is behind may not change
  there.
  A slice's bounds check reads its length where it is indexed, before the place is used:
  `&mut list[i]` with `list: &mut [T]` shallow-reads `*list` (rustc's `&raw const (fake)
  *list`), E0503 while `list.iter_mut()` is still live; a slice behind a `Copy` reference
  reads the reference. An array's length is its constant, read for nothing but its
  initialization.
- Unaligned references (E0793): a `&` or `&mut` of a field a `#[repr(packed)]` struct may
  leave less aligned than its type needs, as rustc's `check_packed_ref` (not `&raw`, nor
  behind a pointer; a field of a type parameter or of a `dyn` tail may be unaligned).
  `offset_of!`, which lowering computes as the address of a field of a value at address 0,
  is rustc's `NullOp::OffsetOf` and takes no reference.
- Builtin derives are checked as rustc expands them (`rustc_builtin_macros/src/deriving`;
  `blaze-engine borrowck` expands them, as `blaze check` and builds do). The methods derived for
  a `#[repr(packed)]` struct copy each field out of it, `{ self.x }` at the field, rather
  than borrow it, so one that is not `Copy` is E0507 where the field is written; `Debug`
  takes the last field by one more reference, so an unsized one still becomes a
  `&dyn Debug`, and every derive but `Copy` and `Default` bounds the struct's type
  parameters by `Copy` too (`needs_copy_as_bound_if_packed`). A derived impl has the
  struct's lifetime parameters, so its self type elides none (E0726).
- Conflicting borrows: shared, mutable, two-phase (`vec.push(vec.len())`), shallow borrows of
  match scrutinees, and fake reads (E0499, E0500, E0501, E0502, E0503, E0506, E0524). The drop
  of the old value before an assignment is the assignment's write, so assigning to a borrowed
  place is E0506 rather than E0597. A compound assignment's read and write are one access
  unless the place goes through a field whose type has lifetimes (`*s.pointer += 1`), where
  rustc reports a conflict of each. A `match` reads a discriminant, and switches on a `bool`,
  integer or `char` it tests against a literal, at what it matches rather than at the arm.
- Errors for a borrow that does not live long enough, as rustc chooses among them: E0713 when
  the borrow is of a field of a value whose type implements `Drop` and the drop is of that
  value, E0373 ("may outlive the current block") for a closure a `let` binds as a block's
  value, and E0597 rather than E0515 for what a function returns as an `impl Trait` that
  captures no lifetime (rustc's `OpaqueType` constraint), even when a `move` closure in it
  borrows a variable it took by value; but E0515 where liveness keeps no region of the
  hidden type live (an alias whose bounds say it outlives `'static`), as rustc then blames
  the return. A borrow an `async` closure captures is blamed as any borrow of a local
  (E0515 when it is returned, not E0373), as rustc's `borrow_spans` knows captures only of
  closures and coroutines. The mutable borrows of a variable
  declared without `mut` are one E0596, at the declaration once there are two.
- `'static` in what a type or signature spells (`x: &'static u32` in a struct, a parameter
  `&'static T`) is related as any other region; a copied value proves its type `Copy`
  (`impl Copy for Foo<'static>`); a loop's way back may unwind as rustc's `FalseUnwind`, so a
  borrow that must outlive the function is reported in a loop that never ends; and
  `dyn Any + 'r` in the signature implies `'r: 'static`, which `trait Any: 'static` requires,
  of a supertrait too (salsa's `dyn Database` through `ZalsaDatabase: Any`); the types in a
  trait object's arguments imply what they do (`&dyn Fn() -> &'a G<'db>` that `'db: 'a`).
  The type an object binds an associated type to is invariant (`dyn Trait<Out = &'a u32>`),
  and unsizing to `dyn Trait` proves the value's type implements `Trait`, with what its impl
  needs of the regions and what the trait says its types outlive (`T` as `dyn Any + 'a` needs
  `T: 'static`); a field's `dyn Any + 'a` implies nothing of `'a`, as rustc's inferred
  outlives leave out what mentions `Self`. A returned associated type whose impl gives it a
  lifetime nothing constrains (`impl<'a, T: 'a> Tr for T { type A = <T as Mk<'a>>::R }`) is
  that lifetime a region of its own, live where the return slot is, as rustc normalizes the
  return slot's type. An associated type in a signature is related as what it normalizes
  to, so `for x in &mut v` keeps `v` borrowed in the `IterMut<'a, T>` that
  `<&'a mut Vec<T> as IntoIterator>::IntoIter` is; two associated types are too (a GAT's
  `Self::Item<'w, 's>` and `Self::Item<'w, 'static>` are both `&'w T`), with what a where
  clause makes of their lifetimes (`T: Fun<F<'a> = [u8]>` makes `T::F<'x>` `[u8]` only where
  `'x` is `'a`). The checker's where clauses that name no parameter are normalized first, as
  rustc normalizes a param env. Unsizing an array to a slice keeps its
  element type (`[T; N]: Unsize<[T]>`), so `&mut [&'a T; 1]` as `&mut [&'b T]` makes `'a`
  and `'b` one; unsizing inside a struct (`&RefCell<T>` to `&RefCell<dyn Read>`) unsizes the
  argument its last field does and keeps the others. A closure typed against an expected
  signature with no `for<'a>` has it, so its parameters' types are the `Fn` bound's arguments
  (`|z: &[i32]|` passed as `F: FnOnce(S)` takes the `S` it is called with). A variable with a
  written type is related to it as rustc's user type annotation, so an error through it is
  blamed on the annotation. The lifetime a return type elides is the one lifetime the
  parameters' types write or elide, as rustc's name resolution finds them in the written
  signature: a `'static` in a type alias's expansion (`id: Id` of `type Id = IdLt<'static>`)
  is not counted, nor the lifetime `&dyn Trait` leaves out of its object, only what its
  bounds write (`dyn Trait<'_> + 'a`). A parameter that elides one lifetime has
  it once however often its type does (`s: S2` with `type S2<'a> = S<'a, 'a>`); one that
  writes `Self` gives elision none of the impl's lifetimes (`self` of `impl Id<'_>`), and
  `Self::Target` in an impl for `S<'_, '_>` has the impl's lifetimes. `Self` written in a
  trait object's or `impl Trait`'s bounds or a function pointer's signature is the impl's self
  type too, its anonymous lifetimes the impl's own (`f: &mut dyn FnMut(&mut Self)` in
  `impl Builder<'_>`), as rustc makes each `'_` of an impl header one parameter of the impl.
- Raw pointers: a reference coerced to one relates what it points to, raw and function
  pointers compare when they have a common supertype, an array's pointer cast to its
  element's relates the element, and a cast between pointers to trait objects keeps the
  principal trait's arguments and may only shorten the object's lifetime. A static is read
  through a reference to its type with every lifetime `'static`, mutable for a `static mut`
  (rustc's `*mut T`), so `BAR = &n` and `FOO(&n)` are E0597. `Fn::call` through a `dyn Fn`
  matches its tupled signature to the arguments one by one. E0521 in a function needs the
  escaping lifetime in a parameter's type, else it is "lifetime may not live long enough".
- The signature's associated types are normalized as rustc's universal regions normalize
  them: what the impl of one needs of the regions must hold, reported at the signature
  (`-> <&'a () as Visitor<'d>>::Value` with `impl<'a, 'd: 'a> Visitor<'d> for &'a ()` and
  `'a: 'd` makes `'a` and `'d` one, which the signature does not say). So are those of the
  where clauses of a struct or enum the signature names, as rustc's
  `compute_implied_outlives_bounds_inner` normalizes what each type's well-formedness asks:
  `foo: Foo<(&'a str,)>` of `struct Foo<T: Trait> where T::Assoc: Clone` with only
  `impl<X: 'static> Trait for (X,)` needs `'a: 'static`.
- Well-formedness in bodies: a called function's signature as written, before its associated
  types are normalized, and its generic arguments are well-formed at the call (`&'a &'b ()`
  that `'b: 'a`); so is a function item's signature where it becomes a function pointer, with
  its late-bound lifetimes instantiated. A function item is well-formed with its where clauses
  and return type, and an associated type `<T as Trait>::Ty` where `T` implements `Trait`
  (`impl Trait for &'static ()` makes `<&'a () as Trait>::Ty` need `'a: 'static`). The
  generic arguments a path spells out are well-formed, and a called function's where clauses
  hold for them, in code that is never reached too. A constant or function named through a
  type that spells out lifetimes, `A::<'a>::C` or `<A<'a>>::new`, has its impl's self type (or
  for a trait's item, `Self`) equal to `A<'a>`, in a range pattern's bounds too.
- Promotion: a borrowed function item, and a value whose qualifs are empty, asked of the value
  rather than its type as rustc's `check_consts::qualifs` (`NeedsDrop`, `HasMutInterior`;
  `hir-ty-borrowck/src/qualifs.rs`): an aggregate has its parts' qualifs unless it is a union or
  its type has a `Drop` impl or is `UnsafeCell`, a named inherent or free `const` what its own
  value has (`mir_const_qualif`), a borrow what the place it borrows has (`&STATIC` none; an
  autoref too, so `Droppy.get_ref()` does not promote a `Droppy` with a `Drop` impl), and
  anything else (a call, a literal) what any value of its type has. So `&[OPEN_PAREN, INDENT]`
  is promoted though another variant of their enum holds a `Cell` or a `Box` (ruff's printer),
  and `&Signals::EMPTY` though `Signals` holds an `Arc`. In a `static`'s value a static that is
  neither `extern` nor thread-local may be borrowed in a promoted constant
  (`static M: Meta = Meta::new(&[(&OPT, true)])`, nushell's `#[exp(..)]` tests).
  A mutable borrow is promoted only of an empty array, the one an index or method call makes
  of its receiver too (`&mut [][..]`, `[].iter_mut()`), as rustc's `validate_ref`.
  In a `const` or `static` item's value, a call of a `const fn` is promotable too
  (`Migrations::new(&[Migration::new("a")])`), as it is in rustc's const contexts, a method
  call and a block of items and a tail included: tracing's `fieldset!` names a field
  `{ const NAME: FieldName<..> = ..; NAME.as_str() }` in a `static`. A closure that captures
  nothing is an aggregate of constants, promoted as rustc promotes it (`unwrap_or(&|_| true)`).
- Region inference: liveness of locals (drops kept live only back to where the local may be
  initialized; what a drop uses as rustc's dropck: an associated type as what it normalizes
  to, another alias what its bounds and captures leave, all its arguments under a binder of
  its type (`fn(T::Gat<'a, '_>)`), `for<'a> T: 'a` as `'static`, and
  nothing of a `ManuallyDrop`), outlives constraints from every statement, call, cast and aggregate, the
  signature's universal regions and implied bounds (normalized parameters and the impl
  header included, for associated consts too; a projection implies what its arguments do as
  well as what it normalizes to, `<&'a &'b () as Trait>::Type` that `'b: 'a`, and its trait's
  outlives where clauses, `<T as Trait<A>>::Assoc` that `A: 'static` where
  `trait Trait<A: 'static>`), where clauses (elaborated through
  supertraits) and the region constraints of proving them with the trait solver (a
  callee's `I: IntoIterator<Item = T>` makes what `I` normalizes the projection to `T`, so
  `buffer.extend(data.split(..))` borrows `data` for as long as `buffer` holds it; a bound's
  associated type of a closure's type is normalized before it is proven, so
  `v.iter().map(|l| l.trim()).collect::<Vec<&'a str>>()` borrows `v` for `'a`, though not
  where that type has a trait object, whose lifetime rust-analyzer makes `'static` in an impl
  header that leaves it out; `Trait<'static>` of `trait Trait<'a>: 'a` needs `'static`; a where
  clause or impl that proves a bound relates its regions to the bound's, `&'max G: Get` proving
  `&'min G: Get` makes them equal, and the impl's own `T: 'a` must hold), type tests (an
  associated type `<T as Trait>::Assoc: 'a` as rustc's alias bound: what it normalizes to
  outlives `'a`, or a where clause on the same projection with equal lifetimes or the trait's
  bound says it does, or every argument outlives `'a`, with rustc's choice of which of these
  relates regions and which is only checked; an `impl Trait` likewise, as the arguments it
  captures; an `async` block as what it captures, as a closure), and lifetimes written in `let` annotations (`let _: T = value`
  of any value included, and of each variable a destructuring pattern binds, `y` in
  `let (y, _): (&'static u32, u32)`; a lifetime left out, or a `_`, that an alias uses twice
  is one lifetime, as in `let (y, _z): (&u32, &u32)` written `Pair<u32>` with
  `type Pair<'a, T> = (&'a T, &'a T)`; the variable's type is well-formed, so
  `let z: &'a &u32 = &&y` needs `y` to outlive `'a`), struct and variant patterns that spell out generic
  arguments (`let Foo::<'static> { .. } = foo`) and constant patterns (`<() as Foo<'a>>::C`,
  whose type is a subtype of what it matches, as rustc ascribes it, so it matches a
  `&'static u32` only if `'a: 'static`), casts (the type written, its `_`s holes: `ptr as _`
  relates no lifetime, though inference makes it `'static` from where the value goes), turbofishes (a method's own, `x.f::<&'a T>()`, too) and struct
  literal and variant paths, in closures too. A `_` inside an associated type in any of these
  annotations (a `let`'s, a turbofish's, a closure's parameter or return) is what equating the
  annotation with the inferred type makes it, as rustc solves its user type annotation, so
  the impl the associated type selects has its where clauses proven, and the value has the
  annotation normalized: `let _: <(_,) as Tr>::Ty = a` with `impl<T: 'static> Tr for (T,)`
  needs `A: 'static`, and `Some::<<(u8, _) as Tr>::Ty>(&temp())` needs the borrow to be
  `'static` (E0716). `Self { .. }` and `Self(..)` in an impl have the
  impl's self type, with its anonymous lifetimes (`impl Foo<'_>`) as the impl's own. A function item called through a variable
  (`let f = foo::<&'static u32>; f(&x)`, or through a reference, `let f = &foo::<..>`) has the
  variable's type, and the variable is used where it is called (a captured constructor too).
  A closure called through a variable takes its arguments as its signature says, whose
  lifetimes rustc infers once, in its type (`let f = |_| {}; f(&x)` in a loop needs `x` to
  live as long as `f`), unless it writes a parameter's or its return's type. `self` of a
  method of `impl dyn Trait` is `dyn Trait + 'static`, as the impl's header defaults it. A struct literal proves the struct's where clauses, and
  the generic arguments its path spells out are well-formed (`Outer::<T, Inner<T>> { .. }`
  with `struct Inner<T: 'static>` needs `T: 'static`), an array
  repeat `[x; N]` of more than one copy proves `x`'s type `Copy`, and a path that names an
  associated item through a type (`Foo::f`, `<()>::f`, `T::f`, not `Default::default`) has its
  generic arguments and its impl's self type and trait arguments well-formed, as rustc's user
  type annotation on it. A function item, closure or safe function pointer cast to a
  function pointer is a subtype of it by signature. Two function item types are related with
  the item's variances, as rustc's `variances_of` infers them from its signature (`fn foo<'a>()
  -> PhantomData<&'a ()>` is covariant in `'a`; a constructor `fn(T) -> S<T>` is invariant in
  `T`, not as `S`); a variable holding one is copied into a temporary of its own type to be
  called, as rustc's MIR does, and the type a path's written arguments give a callee
  (`f::<&'a T>`) is related to the callee's own at the path, blamed after the other
  interesting constraints (rustc's `AnnotationSource::GenericArg`). A where clause `for<'a> T: 'a` is
  `T: 'static`, and a type parameter that fails it is rustc's codeless "does not live long
  enough". A trait object cast to one of its supertraits' (`dyn Foo<'static>` to
  `dyn Bar<'static, 'a>` with `trait Foo<'a>: Bar<'a, 'a>`) has the supertrait as the
  source's arguments instantiate it equal to the target's.
- Higher-ranked bounds, as rustc's placeholders: a `for<'a>` predicate is proven for a
  placeholder `'a`, and so is one an impl's where clauses need on the way
  (`T: DeserializeOwned` through `T: for<'de> Deserialize<'de>`). A placeholder is a universal
  region that outlives only itself, so one the impls make outlive something else is rustc's
  "higher-ranked lifetime error": `&'static str` is `Deserialize<'de>` only for
  `'de: 'static`, and a forwarding impl that needs `T: for<'a> Bar<&'a isize>` of a `T` that
  is only `Bar<&'b isize>` needs `'a: 'b`. A region that must outlive a placeholder outlives
  `'static`, as a region of a lower universe does in rustc, and so does a type, whether the
  placeholder is the predicate's own or one of a `for<'a>` in the where clauses that prove it:
  `Eat<&'1 mut I, F>: Iterator` needs `for<'a> F: FnMut(<&'1 mut I as LendingIterator>::Item<'a>)`,
  whose normalization needs `&'1 mut I: 'a`, so `'1: 'static` (rustc's "due to a current
  limitation of the type system"), and a type parameter that must outlive one is rustc's
  codeless "does not live long enough". A closure or coroutine is made where its creator's where
  clauses hold, so they are proven again at its creation with its parent's arguments (rustc's
  `prove_aggregate_predicates`): `for<'a> &'a S: 'a` needs `S: 'static` there, and a closure
  of `negotiate<S>` is "`S` does not live long enough". What a parameter's type implies is
  assumed, but its higher-ranked where clauses are proven, as rustc applies what proving the
  type's well-formedness needs: `Foo<&'lt u8>` with `for<'x> T::Ty<'x>: Sized` normalizes
  `<&'lt u8 as Trait>::Ty<'x>`, whose `Self: 'x` is `'lt: 'static`. Function pointer types
  are related as rustc's higher-ranked subtyping (`TypeRelating::binders`): the more general
  side's `for<'a>` is instantiated with placeholders of a new universe, the other's with
  variables of it, both ways when invariant; a region of a lower universe made to outlive such
  a placeholder outlives `'static`, and a placeholder made to outlive anything is rustc's E0308
  "one type is more general than the other". So a closure parameter `x: fn(&u32)` whose
  expected type is `fn(&'1 u32)` needs `'1: 'static`. A function item cast to a `for<'a> fn`
  keeps its own `for<'a>`, as rustc's reify, rather than its late-bound lifetimes as variables,
  and a closure cast to one is its signature's `fn` pointer, related the same way. The item's
  elided `&mut dyn Trait` is `&'a mut (dyn Trait + 'a)` there, rustc's object lifetime default,
  so `fn f(_: &mut dyn Trait)` is a `fn(&mut dyn Trait)` (ruff's `record: T::record`). A projection
  rust-analyzer normalized under a `for<'a>` stays as written in a `let`'s type: rustc
  normalizes it for the placeholder, so `let _: for<'a> fn(<&'a () as Trait>::Ty) = |_| {}`
  with only `impl Trait for &'static ()` needs the placeholder to be `'static`, rustc's
  "not general enough" (its type op's error, which its type check cannot see) rather than E0308.
  rust-analyzer gives a trait object
  behind an elided reference in a signature `'static`, not the reference's lifetime, so a
  placeholder the solver equates with an elided lifetime is left out
  (`fn(&mut dyn Write)` for `impl FnMut(&mut dyn Write)` holds).
- Errors on universal regions: "lifetime may not live long enough", E0521, E0309, E0310,
  E0311, with rustc's object lifetime defaults (`&'a dyn Trait` is `dyn Trait + 'a`, a
  closure's parameter `|x: &dyn Trait|` too, a trait
  bounded by `'static` makes its objects `'static`, in a closure's parameter as well (salsa's
  `cycle_fn = |db: &'db dyn Db, ..|`), elsewhere in a body the lifetime is
  inferred). Of universal regions made to outlive each other, as rustc only the first made
  (a closure's creator's before its own) is checked against all it outlives, the others only
  against it, so the error is the one rustc reports.
- Loans that outlive their storage: E0597, E0515, E0716, and promoted constants (calls of
  `#[rustc_promotable]` functions such as `size_of` included) and extended temporaries of
  `const` and `static` items as `'static` (not a variable the value borrows, which is E0597
  there, as rustc's `UseAsConst` rather than E0515). E0716 of what a pattern borrows
  (`ref m` in a `match` on a temporary) is at the temporary, as rustc reports it. An expression statement's value is not read once
  its temporaries are dropped, so `&temp();` alone is no E0716, as in rustc. A temporary
  borrowed mutably through a field (`&mut (1, 2).0`) is not promoted, as rustc's
  `promote_consts`. On an unwind path the drop of a local with no drop glue ends nothing,
  as rustc's cleanup blocks drop only what has glue and end no storage: a counter declared
  after the callbacks that borrow it stays borrowed while a panic drops them.
- `impl Trait` captures: a return-position `impl Trait` keeps alive and relates only the
  lifetimes it captures, those listed in `use<..>` or, before edition 2024 outside traits and
  `async fn`, those its bounds name; its type parameters are always captured. The lifetimes
  elided in the function's parameters are among them: from edition 2024 all are captured,
  before it the one lifetime elision gives the output when the bounds write `'_` or elide it
  (`impl Iterator<Item = &i32> + '_`), and `use<'_>` lists that one. So mutating `self` while
  an iterator `fn evens(&self) -> impl Iterator<Item = &i32> + '_` returned is live is E0502.
  In a trait or its impls the trait's or impl's own parameters are always captured, whatever
  `use<..>` says, as rustc relates the `impl Trait` there (a GAT of the trait) invariantly
  in all of them.
- `impl Trait`'s hidden type: each of its regions must be `'static` or one of the lifetimes
  the `impl Trait` captures (rustc's member constraints), else E0700 at the signature; and it
  must outlive what the bounds say (`impl Copy + 'a`). The same holds of an `impl Trait` an
  `async fn` declares it returns, the future's output (whose hidden type's lifetimes are the
  member constraints' to choose, not those type inference unified), in the body that makes the future,
  with the errors at the function's block as rustc's; only the future captures every
  lifetime. A C variadic's `VaList` lifetime is never captured. A trait object in the hidden
  type has its lifetime inferred, and a borrowed capture of a closure or `async` block
  returned through an `impl Trait` that captures no lifetime is E0597, not E0373, as rustc's.
  rustc picks the captured lifetime a
  region becomes and checks the body with that choice; blaze only tests that one exists, so
  it reports E0700 where rustc does and leaves the errors that choice causes unreported.
- A return type with `_` in it is rustc's E0121, which takes the type the body infers: the
  return slot is checked as any other local, its regions inferred, not tied to a signature.
- Closures: captures, E0373, `FnMut` escapes, E0596 and E0594 through captures. A capture's
  borrow is reported at the first use of the place in the closure (rustc's `path_expr_id`,
  which `compute_min_captures` keeps from the capture recorded first). An `async`
  closure's captures flow into its upvars as a closure's do, and its body knows its upvars
  outlive the borrow of its environment (rustc's `compute_components` of a
  `CoroutineClosure`), so an `AsyncFnMut` future may reborrow a `&mut` capture. A closure's
  body has two kinds of universal region, as rustc's: its own (the late-bound lifetimes of
  its signature, `for<'a> FnOnce(&'a T)`, and the borrow of its environment) and its
  creator's (every other region of its type, and the enclosing item's lifetime parameters,
  late-bound ones and the anonymous ones of its impl, `impl Foo<'_>`, included, so `foo::<&'a u32>(&local)` in a closure needs `local` to outlive
  the creator's `'a`). Its parameters and return slot have the types written for them
  (rustc's `check_signature_annotation`: `|x: &'a i32| -> &'static i32 { x }` needs
  `'a: 'static`), and a closure typed without an expected signature has the lifetimes elided
  in its parameters' types as its own, as rustc's signature binds them, so
  `let f = |x: &i32| -> &i32 { x };` is an error.
  One of its own made to outlive what it is not known to is an error in the closure
  (`with(|x| x)` returning the borrow it was given), named E0521 or a `FnMut` escape as
  rustc names it: E0521 only when a captured variable's or a parameter's type names one of
  the two lifetimes, else "lifetime may not live long enough". A borrow through a variable
  the closure captures by reference outlives the closure's borrow of its environment, so
  returning `&mut *captured` from an `FnMut` closure is the escape rustc reports. A local
  with a written type that a value outliving a universal lifetime is assigned to is blamed
  as rustc blames a type annotation, not the assignment. What a closure needs of its creator's regions is a requirement the creator
  proves where it makes the closure (closures are checked before the bodies that make them),
  so the error is the creator's, as in rustc. Closures, `async` blocks and `const { .. }` blocks in code that is never
  reached (after a `return`) are checked too, as rustc checks every body nested in an item's
  (`nested_bodies_within`), though MIR lowering leaves the code that makes them out. So is a `T: 'a` the closure cannot prove: the
  creator proves `T` outlives each of its regions that outlive `'a`, and an error is
  reported where the closure needed it.
- `const { .. }` blocks: MIR lowering makes one a constant with a body of its own, checked
  with its function as rustc checks an inline const, as a closure is: the regions of its type
  and its function's lifetimes are its creator's, and what it needs of them is proven where
  the function evaluates it (`const { InvariantRef::<'a>::NEW }` compared with a borrow of a
  local needs the local to outlive `'a`). What its value borrows becomes part of the value,
  as for a `const` item, but no call in it is promoted.
- Constants of a signature: an array length or a const generic's default written as a
  block, `[u8; { .. }]` or `struct X<const N: usize = { .. }>`, has a body no item owns,
  checked as rustc checks an anonymous constant, by itself, with its errors in the item
  (`const N: usize = { let x; x }` is E0381).
- `async` blocks and `async fn` bodies: checked as rustc checks a coroutine, as a closure
  of the body that makes it, for moves, initialization and loans across `.await`. Every
  region of the future's type is its creator's, and an `async fn` relates the future's
  output to its declared return type with lifetime elision applied (the lifetime of the
  one `&Self` in `self: Box<&Self>` or `Pin<&mut Self>` too), as rustc proves the
  `Future<Output = ..>` bound of the `impl Future` it returns.
- Items, as rustc's `wfcheck` (the crate `hir-ty-wfcheck`, next to the checker): a struct's,
  enum's or union's fields must be well-formed given its where clauses and what rustc infers
  from the fields (every `T: 'a`, never `T: 'static`), so `&'static T` or `Bar<T>` with
  `struct Bar<T: 'static>` in a field needs `T: 'static` written (E0310). What the impl of an
  associated type in a field needs is not inferred, so `<T as Dummy<'a>>::Out` with only
  `impl<'a, T: 'a> Dummy<'a> for T` needs `T: 'a` written (E0309). A trait impl's
  associated types must be well-formed given the impl's where clauses and what its header's
  types imply (E0309), and the impl must satisfy the trait's outlives bounds, as
  `trait Foo: 'static` (E0310); a generic associated type is checked with its own where
  clauses, not the trait's. The where clauses of an ADT, impl, trait or function must be
  well-formed given its bounds and what its signature's and impl header's types imply
  (`fn foo<'a, L: X<&'a Nested<K>>>()` needs `K: 'a`). A function's signature and an impl's
  header are well-formed too: the trait bounds of each struct or enum they name, and the trait
  of each associated type, hold, so what the impls that prove them need of what outlives what
  must hold: `-> ServiceChainBuilder<ServiceChain<P, S>, NS>` with `struct
  ServiceChainBuilder<P: Service, ..>` and only `impl<P: Service, ..> Service for
  ServiceChain<P, S> where P::Error: 'static` needs `P::Error: 'static` (E0310, of an
  associated type, which outlives a region where a where clause or its trait's bound says so,
  or where its arguments do), and `impl<X> TestTrait for [Foo<(X,)>; 1]` with `struct Foo<T:
  Trait>` and `impl<X: 'static> Trait for (X,)` needs `X: 'static`. A type that outlives a
  region only if a lifetime in it does is E0477 ("does not fulfill the required lifetime"):
  `foo: Foo<(&str,)>` there. As rustc, an item that also needs a lifetime to outlive one
  it does not (E0491) gets only that error, which blaze does not report yet. A function's
  signature, with its impl's header, is normalized as rustc's `WfCheckingCtxt` normalizes it,
  in an inference context of its own, once for what it implies and once for its
  well-formedness, so a lifetime normalizing makes is a region variable of its own in each:
  `&'a mut self` of an impl for `QueryTable<'me, Q, <Q as QueryBase>::Db>` with `Q: for<'f>
  AsyncQueryFunction<'f>` (whose supertrait is `QueryBase<Db = <Self as
  AsyncQueryFunction<'f>>::SendDb>`) implies `<Q as AsyncQueryFunction<'?1>>::SendDb: 'a`
  and needs `<Q as AsyncQueryFunction<'?2>>::SendDb: 'a`, which does not hold (E0309). An
  associated type with such a variable in it outlives a region as rustc's
  `alias_ty_must_outlive` says: with no bound from the where clauses or its trait, even
  one of other lifetimes, when its arguments do, a variable being made to; with one, only
  by a bound of the same type or by its arguments, a variable outliving nothing. What
  normalizing a signature needs of what outlives what is needed too: `fn as_ref2<'a>(&'a
  self) -> Self::Output<'a>` through an impl whose where clause `T: AsRef2<Output<'b> = ..>`
  asks the GAT's `Self: 'b` needs `T: 'b` (E0309). As rustc's `assoc_def`, an item that an
  impl with a type or const parameter it does not constrain (E0207) defines is never
  projected to: the projection is an error, and a function whose signature normalizes to
  one is not borrow-checked, as rustc's writeback taints its body.
  The type a returned `impl Trait` hides must be well-formed and outlive what its bounds
  say, given what the function knows (rustc's `check_opaque_meets_bounds`): `-> impl Sized + 'static` hiding
  a `T` is E0310 at the `impl Trait`; and well-formed where the body defines it, as rustc's
  `add_item_bounds_for_hidden_type` requires (#114728). In its defining body the type it hides is
  also well-formed at the signature, as rustc's implied bounds normalize the returned `impl Trait`
  to it there and prove what that asks: `-> (Box<dyn Any>, impl Sized)` hiding
  `Outlives<'static, <T as Trait>::Assoc>` needs `<T as Trait>::Assoc: 'static` (E0310 at
  the function), the type around it (`&'t impl Trait`) still assumed. Its trait and
  associated type bounds hold of the type
  it hides where the body returns it, as rustc proves them there: `impl Iterator<Item =
  Box<dyn Foo>>` hiding `Iter<'a, T>` needs the where clauses of that `Iterator` impl with
  `'a` `'static`. A closure or coroutine outlives as its captures do, and
  an `impl Trait` with no outlives bound as its arguments. An `impl Trait` a trait's method
  returns, an `async fn`'s future included, is an associated type of the trait whose bounds
  must be well-formed given only what the method's parameters imply (rustc's
  `check_associated_type_bounds` of the RPITIT): `async fn foo(&self) -> &(T, U)` in
  `trait MyTrait<T, U>` needs `T` and `U` to outlive the lifetime of `&self` (E0311), and
  `async fn foo() -> &'static impl T` needs `Self: 'static` (E0310).
- Function items and locals, as rustc's type check: a function item used as a value, not
  only called (`let f: fn(&'a str) -> _ = extend;`, or passed to a call), needs its where
  clauses and its type arguments well-formed, and so does a function an inline `asm!` names
  by `sym`, with the lifetimes its path writes (`sym dep::<'a, T>` with `fn dep<'a, T: 'a>`
  needs `T: 'a`, E0309); a variable's or temporary's type is `Sized`,
  which a where clause `<&'static T as A>::X: Sized` proves only for `'static`.
- E0621 ("explicit lifetime required") where rustc gives it: for an anonymous lifetime a
  parameter's written type has, not in a trait impl's method, nor for one of `self`, of the
  return type or of an impl's header.
- Where errors are: at the expression rustc points to. A borrow `format_args!` (so
  `println!`, `format!`, `write!`) makes of an argument has no syntax of its own, so its
  errors are at the argument, or for a variable the template names (`"{x}"`) at the
  template; one at what else it lowers to, as its call of `Arguments::new`, is at the macro
  call. An error in an expression a macro writes (`&$e`) is at the macro's definition
  when it is in the same file, as rustc reports it there; one in what the caller wrote is at
  the caller's. A variable a macro of another crate declares, as `pin!`'s, is a temporary in
  the messages, as rustc's `describe_place` leaves it unnamed (E0716, not E0597).
- `unused_mut` ("variable does not need to be mutable"), a warning: a binding declared `mut`
  that no assignment or mutable borrow needs the `mut` of, as rustc's `used_mut`. A closure
  needs the `mut` of a captured variable when its body mutates it, and a name that starts
  with `_` is left alone, as rustc does. A body whose types have an error gets none, as
  rustc lints no body its type check failed. A source that denies it fails the build with it
  ([no-rustc.md](no-rustc.md), step D), unless `BLAZE_BORROWCK_REFUSE=0`.

## How it is tested

`harness/borrowck_ui.py <rust checkout>` scores it against rustc's own UI tests at the
pinned nightly's commit: everything under `tests/ui/borrowck` and `tests/ui/nll`, every test
with a Polonius revision, and every other UI test whose expected output has a borrow-check
error. Each revision of a test is a unit, read with the `--cfg`s rustc gets for it. What a
unit should give is what the pinned nightly rustc gives on it, once with Polonius and once
without; those runs are checked against the tests' own `//~ ERROR` annotations so that a
harness mistake shows.

A unit passes when blaze's set of borrow-check errors, as (error code, line), is rustc's
set. Errors rustc gives no code are named by their message. A unit is out of scope, with a
counted reason, when it cannot say anything about the borrow checker: it needs a nightly
feature, an auxiliary crate or another file, a `-Z` flag other than Polonius, rustc
stops before borrow checking, or rust-analyzer reports an error in it that rustc does not.
For those, rust-analyzer's type checking follows rustc's in what it reports:

- No "type annotations needed" (E0282) in a body that already has an error, as rustc's
  `InferCtxt::tainted_by_errors` suppresses it at writeback and where a type must be known
  (`hir-ty/src/infer.rs`, `InferenceDiagnostic::taints_body`).
- A binary operator applied to types with no impl of its trait is an error (E0369, E0368
  for `op=`) that taints the body, as rustc's `check_overloaded_binop` reports it
  (`hir-ty/src/infer/op.rs`).
- A closure required to implement a `Fn*` or `AsyncFn*` trait its closure kind does not
  allow is E0525, not an unsatisfied bound, as in rustc's
  `emit_specialized_closure_kind_error` (`hir-ty/src/solver_errors.rs`).
- A name a pattern binds twice in one product context (`(a, a)`, `a @ b @ a`) is two
  bindings and an error, and a later use means the last one; the alternatives of an
  or-pattern bind the same one. This is rustc's resolver `fresh_binding` with its product
  and or contexts (`hir-def/src/expr_store/lower.rs`, `BindingList`).

With no borrow checker, the 246 units rustc accepts would pass. Now:

| Mode | In scope | Pass | rustc accepts, blaze too | rustc rejects, same errors |
|---|---|---|---|---|
| Polonius | 1590 | 1587 (99.8%) | 246 of 246 | 1341 of 1344 |
| NLL | 1588 | 1585 (99.8%) | 227 of 227 | 1358 of 1361 |

460 units are out of scope; 346 of them need a nightly feature (at 68be5dec, nightly-2026-10-27,
rustc's tests at 12433c30). CI runs the scoring on Linux
and fails below a floor a few units under the count there, on any error in a unit rustc
accepts, and on any unit the checker panics on, as a panic hides every error in its file.

The other check is that it finds nothing in code rustc accepts.
`blaze-engine borrowck --project DIR` checks a cargo workspace; its last line counts the files,
errors and files it panicked on; a panic makes it fail. It refuses a workspace `cargo metadata`
fails on: rust-analyzer would read it with `--no-deps`, whose crates have no dependencies, and
every type of another crate would be an error the bodies using it do not show.
`harness/borrowck_apps.py` runs it over every application of `apps/apps.json` at its pinned rev,
with what `cargo metadata` needs that a clone lacks (bun's vendored crates, which its own build
fetches), plus the harness's `corpus` and `mixfutures` crates (which CI checks too), and lists
each app's errors and panics. At 68be5dec all 22 applications, `corpus` and `mixfutures` check
clean; at 47793534 nushell (53, E0716 in its tests), ruff (19, E0716 and "lifetime may not live
long enough") and rust-analyzer (1, E0515) got errors rustc does not give.

## What is left

Three units fail in each mode, all an error rustc gives that blaze does not:
`higher-ranked-auto-trait-11#no_assumptions` and `implementation-not-general-enough-ice-133252`
(the first cause below) and `supertrait-mentions-GAT` (the second; rustc reports it on the dummy
span of an obligation of its own, where no item's well-formedness check is). The other causes
are what the rules still leave out where no failing unit shows it now:

- **Higher-ranked regions and placeholders**: auto traits of a coroutine's witness (`T: Send`
  of an `async` block holding a `for<'a>` future) are not proven with placeholders.
- **Well-formedness of items** (most of what is left of E0309 and E0310): an impl whose
  header has an associated type is not checked itself (its functions are), nor a `for<'a>`
  bound's projection under its binder (a supertrait
  `for<'a> GatTrait<Gat<'a> = T>` needs the GAT's `Self: 'a` for every `'a`, E0311).
- **Member constraints of `impl Trait`**: the choice rustc makes is not applied, and a
  projection bound that names a lifetime (`impl Iterator<Item = Box<dyn Foo>>`) leaves them
  out.
- **Captures of an empty `match`**: rustc's capture analysis has a closure that matches
  on `x` with no arms read `x`; rust-analyzer's captures nothing there, so the E0381 of an
  uninitialized `x` is missed.
- **Trait selection's region obligations**: an impl's `T: 'static` proven at an unsizing
  cast. A bound on a closure's type is not proven through the solver: rust-analyzer gives a
  closure one signature, `fn(&'1 T)` for one `'1`, where rustc's is `for<'a> fn(&'a T)`, so
  proving it would tie every call's borrow to one region. Only an `Fn` bound with no
  `for<'a>`, on a closure typed against one, relates its parameters.
- **Differences in rust-analyzer's MIR lowering** (below).

## What depends on rust-analyzer's MIR lowering

The borrow checker reads `hir-ty`'s MIR, so some verdicts depend on how that lowers. These
changes to rust-analyzer were needed, each made as rustc does it:

- Two-phase borrows for autoref method receivers (`vec.push(vec.len())`).
- `if let ... && ...` chains branch directly instead of through a boolean temporary.
- A match guard reads what its arm binds by value through a reference, and the arm moves
  it in only once the guard holds. A guard with `let` (`if let` guards and chains) branches
  on each operand as `if` does, so what its `let` binds is initialized where it holds. Its
  temporaries stay the arm's in every edition, as rustc keeps them (dropped when the arm ends
  or before the next arm is tried), where an `if let` before 2024 keeps them to its statement.
  Where the guard fails, the storage of what the pattern bound ends before the next arm, or
  the next way an or-pattern of the arm can match, is tried, as rustc leaves the arm's scope
  there (`in_if_then_scope`). So each way binds a fresh variable, and assigning a binding
  twice is E0384 like any other variable: in `Ok(a @ b @ a) | Err(a @ b @ a)` the alternatives
  bind the same `a` and `Err(..)` assigns it twice.
- Slice pattern indices carry the pattern's minimum length (`ConstantIndex::min_length`).
- Promotion: a division by a constant other than 0 (and -1 for a signed dividend that may be
  the minimum), and `&mut []`.
- Coercion (`hir-ty/src/infer/coerce.rs`): `&T` to `&T` reborrows. rust-analyzer gives elided
  lifetimes in signatures an error region, so two unrelated ones compared equal and the
  reborrow was skipped as a no-op; a closure then captured the reference variable rather
  than what it points to.
- A `let`'s fake read of its value is at the pattern (rustc's `FakeReadCause::ForLet`), not
  the enclosing block, which is where the borrow checker finds the place `let _: T` ascribes;
  `let x = e` has it too, before `e`'s temporaries are dropped, so what `x` borrows from a
  `match` arm's bindings must outlive them.
- A `let`'s temporaries borrowed in a tuple struct's or variant's constructor arguments
  (`let x = Some(&temp());`, `Self(&temp())`) live as long as the variable, as in rustc since
  1.89; through a constructor held in a variable they are still the statement's.
- The `super let`s of `format_args!` and `pin!` extend the temporaries of their initializers
  only when a `let` extends the block they are in (rust-lang/rust#145784); otherwise what they
  bind lives as long as the temporaries of the expression the block is in, so
  `identity(pin!(x))` kept past its statement, and `println!("{:?}", { &temp() })` from 2024
  on, are E0716.
- A function item or a tuple struct's or variant's constructor written as a value
  (`let f = foo::<T>;`) is assigned to where it goes, though zero sized, so a closure that
  captures `f` or a method that takes it by value does not read it uninitialized (E0381).
- A range pattern's bounds carry their expressions' spans, as other operands do, so a
  constant named there is found with the lifetimes its path writes.
- A cast between raw pointers stays a cast where inference finds the types coerce, as rustc's
  `check_cast` keeps it (rust-lang/rust#113257): the pointees may differ only in lifetimes,
  which inference cannot tell apart, and a cast relates them only for trait objects.
- Type inference records the closures it types without an expected signature
  (`InferenceResult::closures_without_expectation`), whose elided parameter lifetimes rustc
  binds in the closure's signature where rust-analyzer's signature has none.
- The trait solver (`hir-ty/src/next_solver/solver.rs`) answers with the region constraints
  of what it proves, as rustc's does, for an interner made with `with_regions()`: the
  borrow checker's. rust-analyzer's dropped them; type inference does not ask for them and
  its answers are cached apart.
- False edges (`TerminatorKind::FalseEdge`): each way a match arm can match is linked to the
  next by an edge only borrow checking follows, and so are an `if let` or `let .. else` to
  its else and an or-pattern's alternatives as it binds, so borrowck cannot tell an arm is
  unreachable or that a guard always runs (`nll/match-cfg-fake-edges.rs`). Arms and
  alternatives after one that cannot fail are lowered too, reached only by these edges, so
  their guards are checked (`&_ => ..., &_ if { b = &true; false } => ...` is an E0510).
  Before the other passes each becomes a jump to its real target, as rustc's
  `CleanupPostBorrowck` does, so what runs is unchanged.
- What a panic drops: the operands of the calls and aggregates being lowered go before the
  temporaries that live to the end of the statement, as rustc keeps them in the scope of the
  expression they are operands of. In `boom().extend(&String::from(..))` a panic in the
  deref drops `boom()` first, so an `impl Trait` it returns keeps no borrow of the string
  alive (`impl-trait/wf-check-hidden-type.rs`).
- Type lowering gives a trait object written without a lifetime, as a type argument, the
  lifetime of its parameter's one outlives bound (`hir-ty/src/generics.rs`,
  `object_lifetime_default`, RFC 599): `Ref<'x, dyn Trait>` with `T: 'a + ?Sized` on `Ref`
  is `dyn Trait + 'x`, not `'static`. In an impl header, a trait object behind a reference
  whose lifetime is elided has that lifetime too (`hir-ty/src/lower.rs`, `impl_header`), as
  rustc makes both one fresh parameter of the impl: rusqlite's `impl Params for
  &[&dyn ToSql]` holds for `params![id, name]` of locals. In a type written in a body (a
  `let`'s type, a cast's), a lifetime left to inference names nothing, so a trait object it
  would default has a lifetime inferred on its own (`hir-ty/src/lower.rs`, `object_default`),
  as rustc's `resolve_object_lifetime_default` stops at `ObjectLifetimeDefault { lifetime:
  None }` or the body and lowering gives a fresh region: `err as &dyn Error` is
  `&'1 (dyn Error + '2)`, so cargo's `source` returns it as `dyn Error + 'static`.
  `Box<dyn Trait>` there is the same, not `'static`; a trait bounded by `'static` still
  makes its objects `'static`.
- A return-position `impl Trait` has a lifetime parameter for each lifetime elided in its
  function's parameters, after the function's own (`hir-ty/src/lower.rs`, `elided_lifetimes`),
  as rustc duplicates them as parameters of the opaque type. In its bounds, `'_` or an elided
  lifetime is the one elision gives the output: the lifetime of `&self`, else the parameters'
  only lifetime. rust-analyzer gives elided lifetimes in a signature an error region, so the
  `impl Trait` could not hold the borrow it was given. Where the function is called, checked
  or type checked, those parameters are the regions its inputs' elided lifetimes become. A
  parameter whose lifetimes are not counted from its written type (`dyn Trait`, `impl Trait`,
  a function pointer, an associated type) adds none, and gives elision no lifetime unless
  `&self` does.
- Type inference types an inline `asm!`'s `sym` of a function as a path to a value, as
  rustc's `check_expr_asm` does (`InferenceResult::asm_sym_tys`), so MIR lowering names the
  function item with the generic arguments the path spells out, and lowers an `asm!` with a
  `sym` even when its template is only a comment, as it lowered no generic `sym` before.
- Type inference takes an inline `asm!` output as a place written, as rustc's
  `check_expr_asm_operand` does, so `out(reg) v[0]` indexes with `IndexMut` and needs `v`
  mutable (E0596).
- A temporary may be unsized, as rustc's `as_temp` makes one of what is moved out of an
  unsized place: a place expression as a statement (`S[0];`, `*s;`) is moved into one, sized
  or not, as rustc's `stmt_expr` does, and so is an operand (`[a[..]]`), so moving out of a
  borrowed `str`, slice or `dyn` is E0507 or E0508 (`dst/dst-index-fail.rs`,
  `unsized/issue-115203.rs`). rust-analyzer refused to lower a body with one, and only
  fake-read an unsized statement. rustc reports E0161 there too, which blaze does not.

These are differences still open, each the cause of some failures, all of them errors
missed rather than errors made up:

- Temporaries of a block's tail expression in edition 2021 are dropped at a different point.
- Comparisons against constant patterns take their operands by value.
- An arm with a guard binds its by-reference bindings before the guard and the rest after,
  in rustc, so what it binds by value is dropped first; rust-analyzer binds them in the
  pattern's order (`dropck/eager-by-ref-binding-for-guards.rs`, where it reports two errors
  rustc does not).
- Unwind paths are not lowered. rustc's MIR gives every call and drop a cleanup path ending
  at `resume`, where any borrow still live that outlives the function's own regions is an
  error (E0597 for `let x = ...; let r: &'a _ = &x; f();`). blaze checks that at each call,
  drop and assert as if the path were there; it would be exact if the lowering added them.

## What it costs

On every edit the daemon runs the diagnostics of the edited files on a thread of its own,
beside the build, so the check adds to an edit only when it takes longer than the build.
For an edit of the corpus's largest file, the check went from a median of 40 ms with the
checker off to 52-74 ms with it on, while the build took 250-400 ms. A cold check of all of
nushell used 10-25% more CPU time. Both were measured on a loaded machine, so read them as
rough.

## Speed

The check runs on every edited file, so its time is on the edit's path. Two things keep it
small: an edit re-checks only the bodies it changed (the query reads each body's source map
relative to its item, which moving the body does not change), and each check should cost a
small fraction of type inference, which it needs anyway.

`blaze-engine borrowck --project DIR --timings OUT.jsonl` goes through every body of a project's own
crates on one thread. It infers and checks each body once cold, paying for the queries the body
is the first to need (`infer_us`, `first_us`), then infers it and checks it in each mode as an
edit of it costs in the daemon: the rest of the workspace answered and the trait solver's cache
empty, as it is in a new revision (`infer_warm_us`, each mode's phases and sizes).
`BLAZE_BORROWCK_ONLY=part` limits it to the bodies whose name contains `part`. Run it inside
`nix develop`: without the toolchain's sysroot most bodies have inference errors and are not
checked. `harness/borrowck_speed.py` runs it over the bench apps and reports totals, p50/p99/max
and the slowest bodies; `--rustc TOOLCHAIN` measures rustc's own `mir_borrowck` and `typeck`
with `-Zself-profile` as the baseline.

Before any speed work, on main 31b3345f (arm64 ix VM, 16 vCPUs): the checker's total
without MIR lowering, beside type inference's, in ms, and its slowest body:

| App | Bodies | Inference | Polonius | max | NLL | max |
|---|---|---|---|---|---|---|
| ripgrep | 2642 | 2176 | 526 (24%) | 33 | 511 (24%) | 35 |
| fd | 268 | 363 | 156 (43%) | 21 | 151 (42%) | 22 |
| bat | 568 | 2824 | 300 (11%) | 58 | 318 (11%) | 61 |
| hyperfine | 306 | 2548 | 183 (7%) | 19 | 176 (7%) | 23 |
| just | 1660 | 1743 | 1086 (62%) | 52 | 1148 (66%) | 94 |
| axum | 1576 | 8257 | 4033 (49%) | 81 | 3994 (48%) | 91 |
| helix | 6025 | 8261 | 3641 (44%) | 248 | 3479 (42%) | 157 |
| nushell | 35844 | 40051 | 23622 (59%) | 841 | 23677 (59%) | 905 |
| biome (first 62445 bodies) | 62445 | 37973 | 20313 (53%) | 1522 | 17887 (47%) | 932 |

Generating constraints, which asks the trait solver about every statement's types, is more
than half of it everywhere (nushell: 13.4 s of 23.6 s), then liveness, the walk and moves.
Those inference times are cold; the checker's ran after them with the trait solver's answers
still cached, which flattered it.

Measured as an edit pays both, with interval sets, SCC region inference and per-block loan
scopes in, and an ADT's inferred outlives a query and `is_copy` asked once per type: the
checker without MIR lowering beside an edit's inference, in ms, and its slowest body.

| App | Inference | Polonius | max | NLL | max |
|---|---|---|---|---|---|
| ripgrep | 658 | 480 (73%) | 9 | 423 (64%) | 8 |
| fd | 204 | 126 (62%) | 19 | 115 (57%) | 18 |
| bat | 306 | 267 (87%) | 67 | 245 (80%) | 66 |
| hyperfine | 220 | 146 (66%) | 17 | 134 (61%) | 15 |
| just | 1173 | 849 (72%) | 24 | 784 (67%) | 24 |
| axum | 4163 | 3604 (87%) | 76 | 3531 (85%) | 75 |
| helix | 3201 | 2157 (67%) | 118 | 1971 (62%) | 112 |
| nushell | 15758 | 18225 (116%) | 828 | 17249 (109%) | 829 |

Generating constraints is still most of it (nushell: 10.6 s of 18.2 s), and the outliers are
the trait solver proving one body's well-formedness and outlives bounds: nushell's
`custom_value_op` (828 ms) and `serve_plugin_io` (356 ms). rustc's own `mir_borrowck` is 44%
of its `typeck` on ripgrep, 31% on axum and 149% on just.

With a trait reference proven by a query, as rustc's canonical `type_op_prove_predicate`: its
region variables numbered in order, so a goal is proven once for all the bodies of one
environment, and an edit proves again only the goals of what it changed. Same measure:

| App | Inference | Polonius | max | NLL | max |
|---|---|---|---|---|---|
| ripgrep | 630 | 409 (65%) | 8 | 359 (57%) | 8 |
| fd | 200 | 111 (56%) | 18 | 101 (51%) | 17 |
| bat | 288 | 234 (81%) | 64 | 214 (74%) | 64 |
| hyperfine | 223 | 125 (56%) | 15 | 113 (51%) | 14 |
| just | 1069 | 715 (67%) | 21 | 659 (62%) | 19 |
| axum | 3920 | 2595 (66%) | 52 | 2519 (64%) | 51 |
| helix | 3178 | 1899 (60%) | 124 | 1709 (54%) | 118 |
| nushell | 15539 | 11981 (77%) | 240 | 10953 (70%) | 224 |

nushell's constraints went from 10.7 s to 4.4 s, `custom_value_op` from 811 ms to 3 ms and
`serve_plugin_io` from 368 ms to 12 ms. A body's first check pays for the goals no body proved
before it, each in a solver cache of its own: less in all on nushell (31.7 s to 28.9 s) and
helix, more on axum (5.2 s to 6.7 s), whose 16-tuple impls each prove goals no other body
does.

With whether a type is `Copy` and normalizing canonical queries too (a type's regions
`'static` for `Copy`, numbered for normalizing, as rustc's `type_op_normalize`), and dominators
by semi-NCA with dominance read from the tree's entry and exit numbers, as rustc's. Same
measure, on main 5b58cc84:

| App | Inference | Polonius | max | NLL | max |
|---|---|---|---|---|---|
| ripgrep | 610 | 325 (53%) | 7 | 280 (46%) | 7 |
| fd | 190 | 89 (47%) | 16 | 81 (43%) | 15 |
| bat | 278 | 197 (71%) | 63 | 180 (65%) | 63 |
| hyperfine | 206 | 81 (39%) | 8 | 71 (35%) | 7 |
| just | 1076 | 602 (56%) | 19 | 546 (51%) | 18 |
| axum | 3887 | 1699 (44%) | 32 | 1642 (42%) | 32 |
| helix | 3130 | 1509 (48%) | 111 | 1334 (43%) | 105 |
| nushell | 14709 | 9686 (66%) | 202 | 8842 (60%) | 203 |

Moves went from 1.7 s to 0.8 s on nushell and from 0.8 s to 0.1 s on axum, and
`stream_parallel_values` (186 ms, one normalization) left the slowest bodies. The first check
on axum went from 6.7 s to 8.3 s: each query has a solver cache of its own, so a nested goal
two of a body's goals share is solved for each. The slowest bodies now are nushell's
`theme_list::list` (202 ms, liveness over 48k points with no loans), `run_http_server`
(127 ms, the walk) and `tool_router` (107 ms, constraints of a body of 57 points).

With what dropping a type uses found as rustc's dropck finds it: over the strongly connected
components of the types it drops, each expanded once, an ADT's part a query in its own
parameters (rustc's `adt_dtorck_constraint`) that a body instantiates, and whether dropping a
type runs code a query per type modulo regions; and drop liveness asking whether a local may
be initialized leaving a block only of the locals drop live there. Same measure, on main
d93d0958:

| App | Inference | Polonius | max | NLL | max |
|---|---|---|---|---|---|
| ripgrep | 583 | 308 (53%) | 7 | 265 (45%) | 7 |
| fd | 178 | 79 (44%) | 12 | 71 (40%) | 12 |
| bat | 269 | 157 (58%) | 34 | 138 (51%) | 34 |
| hyperfine | 208 | 77 (37%) | 9 | 66 (32%) | 8 |
| just | 1064 | 541 (51%) | 19 | 482 (45%) | 18 |
| axum | 3726 | 1012 (27%) | 15 | 945 (25%) | 14 |
| helix | 3006 | 1399 (47%) | 108 | 1227 (41%) | 106 |
| nushell | 14084 | 6502 (46%) | 108 | 5732 (41%) | 108 |

The walk went from 2.6 s to 0.4 s on nushell and from 0.7 s to 0.04 s on axum, whose futures
asked the trait solver of their drop glue on every drop; nushell's liveness from 2.2 s to
1.5 s, `theme_list::list` from 213 ms to 92 ms and `run_http_server` from 128 ms to 12. The
slowest bodies now are `tool_router` (108 ms, constraints), helix's `keymap::default` (108 ms,
liveness over 37k points) and nushell's `ParseError::labels` and `ShellError::labels` (53 ms
each, constraints).

A goal whose answer names a placeholder of a `for<'a>`, or a variable of a universe past the
root, is proven by the query too: its universes are the query's own, which the body makes anew
in the same order and instantiates, as rustc's canonical query response. It was proven again in
the body on every check, and one such goal of rmcp's `#[tool_router]`,
`(Tool, fn): IntoToolRoute<_, AsyncMethodAdapter<..>>`, took nushell's `tool_router` 107 ms; it
is 0.3 ms now. Nothing else moves (tools/bench/results/2026-10-27-borrowck-speed-placeholders).

## Debugging

- `echo file.rs | blaze-engine borrowck --edition 2021` prints a file's diagnostics as JSON.
- `BLAZE_BORROWCK_MIR=<function>` with that command, or with `--project`, prints the MIR
  of a top-level function and its closures.
- `BLAZE_BORROWCK_DEBUG=1` prints each body's locals, universal regions, constraints and
  loans; `BLAZE_BORROWCK_DEBUG=<function>` only those of that function and its closures,
  which a project needs.
