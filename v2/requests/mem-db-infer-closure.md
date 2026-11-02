# mem-db-infer-closure: requests

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8). Files, all in
`crates/hir-ty/src/infer/`: `closure.rs`, `closure/analysis.rs`,
`closure/analysis/expr_use_visitor.rs`. Their owner is lane db, so every change below is a
request; apply them together with `mem-db-infer-ctx`'s (`infer.rs`, `infer/persist.rs`), which
these program against and which X1 to X4 below ask for. Lines are as of 2026-10-29; other agents
are editing these files (`infer.rs` moved by about 50 lines while this was written), so each entry
quotes its "before" code to find it by. The `db` parameters are left exactly as they are.

Notes and the speed-of-light bound: `docs/v2/plan/lanes/mem/db-infer-closure.md` (and "Speed" at
the end of this file).

## The answer in short

`Place.projections` is a frame value while closures are analysed and owned only in the result.
The analysis gets its own frame types (`PlaceBuf<'f>`, `SourceStack<'f>`, `Sources<'f>`,
`PlaceWithOrigin<'f>`, `CaptureInfoBuf<'f>`, `CapturedPlaceBuf<'f>`, `MinCapturesBuf<'f>`); the
result types (`Place`, `CaptureInfo`, `CapturedPlace`, `ClosureData`) are made once, at exact
size, from them:

- `min_captures` right after the last `compute_min_captures` call of `analyze_closure`
  (`MinCapturesBuf::finish`), before anything reads it;
- `fake_reads` where `analyze_closure` stores them today;
- `kind_origin` where `analyze_closure` stores it today.

The only place analysis reads results back is `walk_captures` (a nested closure's captures and
fake reads become the enclosing closure's): it copies them into frame values.

`ClosureData`, before and after (`infer.rs`):

| field | before | after |
|---|---|---|
| `min_captures` | `FxIndexMap<BindingId, Vec<CapturedPlace>>` (hash table + entries + a `Vec` per root, each grown then `shrink_to_fit`) | `MinCaptures { roots: Box<[(BindingId, u32)]>, captures: Box<[CapturedPlace]> }`: 2 blocks |
| `fake_reads` | `Box<[(Place, FakeReadCause, SmallVec<[CaptureSourceStack; 2]>)]>` | `Box<[(Place, FakeReadCause, Box<[CaptureSourceStack]>)]>` |
| `Place.projections` | `Vec<Projection>` (grown by push, then `shrink_to_fit`) | `Box<[Projection]>` |
| `CaptureInfo.sources` | `SmallVec<[CaptureSourceStack; 2]>` | `Box<[CaptureSourceStack]>` |
| `CaptureSourceStack` | `ThinVec` grown by push at 3 or more, `shrink_to_fit` | unchanged type, built by `from_slice` at exact length |
| `Hash`/`Eq` | order-insensitive (`hash_unordered`, `IndexMap`'s `==`) | derived, order-sensitive |

The order of `min_captures` is observable (it is the order of the closure's upvar tuple and of
MIR's captures), so order-sensitive `Eq`/`Hash` is the right comparison for backdating; the
order-insensitive one let two results whose upvar tuples differ only in order compare equal.

A capture's result size drops from about 96 to 56 bytes (`Place` 40 to 32, `CaptureInfo` 48 to
24) and a closure's `min_captures` from 2 + roots blocks (plus growth and shrink reallocations) to
2. The number of allocations of the result is exactly the number `ClosureData::clone` makes,
which is what the parent's budget test 2 compares.

Every reader outside the three files and `infer.rs`/`persist.rs` compiles unchanged:
`MinCaptures::values()` yields `&[CapturedPlace]`, `&MinCaptures` iterates `(BindingId,
&[CapturedPlace])`, `len()` is the root count, and `&Box<[T]>` iterates and derefs to `&[T]` as
`&Vec<T>`/`&SmallVec` did. Checked by `rg -n "min_captures|closures_data|\.projections|\.sources"
crates/`:

| reader | use | why unchanged |
|---|---|---|
| `crates/hir/src/lib.rs:4325-4326` | `data.min_captures.values().flatten().find(..)`, `place.projections.starts_with(&it.place.projections)` | `values()` kept; `Box<[T]>` derefs to `[T]` |
| `crates/hir/src/lib.rs:4372-4377` | `.min_captures.values().flatten().map(..).collect()` | same |
| `crates/hir/src/lib.rs:4464, 4492, 4524` | `self.capture.place.projections.iter()` | `Box<[T]>` |
| `crates/hir/src/lib.rs:4470` | `sources: &self.capture.info.sources` into `&'db [CaptureSourceStack]` | deref coercion from `&Box<[T]>` as from `&SmallVec` |
| `crates/hir-ty-mir/src/mir/lower.rs:1903-1916` | `for (place, _, sources) in &closure_data.fake_reads`, `span(sources)`, `span(&capture.info.sources)` | `&Box<[(..)]>` iterates; `span` takes `&[CaptureSourceStack]` |
| `crates/hir-ty-mir/src/mir/lower.rs:2784, 4338, 4615, 4623-4624` | `values().flatten().next()/collect()/take(n)`, `place.projections.iter()` | same |
| `crates/hir-ty-borrowck/src/lib.rs:696`, `check/explain.rs:462-463`, `names.rs:100` | `values().flatten()`, `.nth(at)`, `info.sources.first()`, `place.projections.iter()` | same |
| `crates/hir-ty-codegen/debuginfo/types.rs:547, 556` | `values().flatten()`, `for projection in &capture.place.projections` | `IntoIterator for &Box<[T]>` (std 1.80) |
| `crates/blaze-items/src/form.rs:1623-1628, 1653-1654` | `data.min_captures.len()`, `for (binding, places) in &data.min_captures { binding.into_raw(); places.len(); for place in places }`, `&place.projections` | `len()` is the root count as `IndexMap::len`; the item is `(BindingId, &[CapturedPlace])`, and `binding.into_raw()` works on the value as on `&BindingId` |
| `crates/blaze-diagnostics/src/lints/bodies/liveness.rs:206`, `liveness/source.rs:226, 256` | `values().flatten().collect()`, `projections.iter()`, `info.sources.first()` | same |
| `crates/hir-ide/src/tests/closure_captures.rs:28, 94, 124-127` | `place.projections.iter()`, `values().flatten()`, `info.sources.iter()` | same |
| `crates/hir-ty/src/infer.rs` `closure_captures_tys`, `closure_captures_captured_tys` | `.min_captures.values().flat_map(\|captures\| captures.iter()..)` | same |

The readers that do change are all in `infer.rs` and `persist.rs` (X2 to X4) and my files.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): a summary of the sections below, each applied with its own line.

## Prerequisites (other parts)

- **mem-db-infer** (`lib.rs`, `Cargo.toml`): `#![feature(allocator_api)]`, `blaze-mem` and
  `hashbrown` (with `nightly`) in `crates/hir-ty/Cargo.toml`, as in `v2/requests/mem-db-infer.md`.
  `arrayvec`, `either`, `smallvec` and `thin-vec` are already dependencies; nothing else is needed.
- **mem-db-infer-ctx** (`infer.rs`): `pub(crate) frame: &'f blaze_mem::Frame` on
  `InferenceContext<'db, 'f>` (R2). `InferenceContext` derives `Debug`, and the delegate methods
  below are `#[instrument]`ed with it, so `frame`'s `Debug` must print (a `&Frame` does if
  `Frame: Debug`; otherwise a hand-written `Debug` that skips it).

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): in the tree: hir-ty's `allocator_api`, `blaze-mem`, `hashbrown` (mem-db-infer 1-2), `frame` on `InferenceContext<'db, 'f>` (crates/hir-ty/src/infer.rs:2026, `#[derive(Debug)]`).

## Cross-part interfaces these files program against

Each is a change in a file of another part; the exact code is given so mem-db-infer can hand it
to that part's owner with the part's own entries.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): X1-X5 below, each in the tree.

### X1. `InferenceContext<'db, 'f>` (mem-db-infer-ctx, `infer.rs`)

As above. Every `impl<'db> InferenceContext<'db>` in my files becomes `impl<'db, 'f>
InferenceContext<'db, 'f>` (entries C0, A0), and every `&mut InferenceContext<'db>` in a
signature becomes `&mut InferenceContext<'db, 'f>` (entries A1, E3).

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): in the tree: crates/hir-ty/src/infer.rs:2026; no `InferenceContext<'db>` header is left in `infer.rs` or `infer/`.

### X2. The result types (mem-db-infer-ctx, `infer.rs` about lines 1240 to 1510)

1. Delete `hash_unordered` (about line 1246, "Hashes an unordered collection the same for equal
   values") and its test module at the end of the file (`mod tests { use super::hash_unordered;
   .. equal_maps_hash_the_same_whatever_their_order .. a_changed_entry_changes_the_hash .. }`):
   `ClosureData` is its only caller.

2. `ClosureData`: before

   ```rust
   #[derive(Clone, PartialEq, Eq, Debug)]
   pub struct ClosureData {
       /// Tracks the minimum captures required for a closure;
       /// see `MinCaptureInformationMap` for more details.
       pub min_captures: RootVariableMinCaptureList,
       ..
       pub fake_reads: Box<[(Place, FakeReadCause, SmallVec<[CaptureSourceStack; 2]>)]>,
       ..
   }

   impl Hash for ClosureData {
       fn hash<H: Hasher>(&self, state: &mut H) {
           let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = self;
           hash_unordered(min_captures.iter(), state);
           fake_reads.hash(state);
           liberated_sig.hash(state);
           kind_origin.hash(state);
       }
   }

   /// Part of `MinCaptureInformationMap`; Maps a root variable to the list of `CapturedPlace`.
   /// ..
   pub(crate) type RootVariableMinCaptureList = FxIndexMap<BindingId, MinCaptureList>;

   /// Part of `MinCaptureInformationMap`; List of `CapturePlace`s.
   pub(crate) type MinCaptureList = Vec<CapturedPlace>;
   ```

   After (the field docs between are kept as they are):

   ```rust
   #[derive(Clone, PartialEq, Eq, Debug, Hash)]
   pub struct ClosureData {
       /// The minimum captures the closure needs, by root variable in the order each was first
       /// captured: the order of the closure's upvars.
       pub min_captures: MinCaptures,
       ..
       pub fake_reads: Box<[(Place, FakeReadCause, Box<[CaptureSourceStack]>)]>,
       ..
   }

   /// A closure's minimum captures by root variable, in the order each variable was first
   /// captured: every root's captures in one block, and where each root's captures end.
   #[derive(Clone, PartialEq, Eq, Debug, Default, Hash)]
   pub struct MinCaptures {
       roots: Box<[(BindingId, u32)]>,
       captures: Box<[CapturedPlace]>,
   }

   impl MinCaptures {
       /// `roots` are the root variables in order, each with the end of its captures in
       /// `captures`.
       pub(crate) fn from_parts(roots: Box<[(BindingId, u32)]>, captures: Box<[CapturedPlace]>) -> Self {
           debug_assert!(roots.is_sorted_by_key(|&(_, end)| end));
           debug_assert_eq!(roots.last().map_or(0, |&(_, end)| end as usize), captures.len());
           Self { roots, captures }
       }

       /// The number of root variables.
       pub fn len(&self) -> usize {
           self.roots.len()
       }

       pub fn is_empty(&self) -> bool {
           self.roots.is_empty()
       }

       /// Each root variable with its captures, in order.
       pub fn iter(&self) -> MinCapturesIter<'_> {
           MinCapturesIter { roots: self.roots.iter(), captures: &self.captures, start: 0 }
       }

       /// Each root variable's captures, in order.
       pub fn values(&self) -> impl ExactSizeIterator<Item = &[CapturedPlace]> {
           self.iter().map(|(_, captures)| captures)
       }

       /// Every capture, root by root: the closure's upvars in their order.
       pub fn flattened(&self) -> &[CapturedPlace] {
           &self.captures
       }

       pub(crate) fn flattened_mut(&mut self) -> &mut [CapturedPlace] {
           &mut self.captures
       }
   }

   impl<'a> IntoIterator for &'a MinCaptures {
       type Item = (BindingId, &'a [CapturedPlace]);
       type IntoIter = MinCapturesIter<'a>;

       fn into_iter(self) -> MinCapturesIter<'a> {
           self.iter()
       }
   }

   /// [`MinCaptures::iter`].
   #[derive(Clone, Debug)]
   pub struct MinCapturesIter<'a> {
       roots: std::slice::Iter<'a, (BindingId, u32)>,
       captures: &'a [CapturedPlace],
       start: u32,
   }

   impl<'a> Iterator for MinCapturesIter<'a> {
       type Item = (BindingId, &'a [CapturedPlace]);

       #[inline]
       fn next(&mut self) -> Option<Self::Item> {
           let &(binding, end) = self.roots.next()?;
           let captures = &self.captures[self.start as usize..end as usize];
           self.start = end;
           Some((binding, captures))
       }

       #[inline]
       fn size_hint(&self) -> (usize, Option<usize>) {
           self.roots.size_hint()
       }
   }

   impl ExactSizeIterator for MinCapturesIter<'_> {}
   ```

   A root with no captures (`end` equal to the previous one) is allowed: persist's test makes
   one, analysis never does.

3. `CaptureInfo`: before `pub sources: SmallVec<[CaptureSourceStack; 2]>,`; after
   `pub sources: Box<[CaptureSourceStack]>,`.

4. `CaptureSourceStack` (about line 1440 on): delete `push`, `truncate` and `shrink_to_fit`
   (their callers become `SourceStack<'f>` in the analysis, `from_slice` and `try_from_fn` below),
   and add:

   ```rust
       /// The stack of `ids`, base first: inline up to two, else one block of exactly their number.
       pub(crate) fn from_slice(ids: &[ExprOrPatIdPacked]) -> Self {
           Self(match *ids {
               [] => panic!("a capture source stack has a source"),
               [one] => CaptureSourceStackRepr::One(one),
               [first, second] => CaptureSourceStackRepr::Two([first, second]),
               _ => {
                   // alloc: a result's stack of three or more, once at its length
                   let mut many = ThinVec::with_capacity(ids.len());
                   many.extend_from_slice(ids);
                   CaptureSourceStackRepr::Many(many)
               }
           })
       }

       /// The stack of the `len` ids `next` reads, base first, at exactly their number.
       pub(crate) fn try_from_fn<E>(
           len: usize,
           mut next: impl FnMut() -> Result<ExprOrPatIdPacked, E>,
       ) -> Result<Self, E> {
           Ok(Self(match len {
               0 => panic!("a capture source stack has a source"),
               1 => CaptureSourceStackRepr::One(next()?),
               2 => CaptureSourceStackRepr::Two([next()?, next()?]),
               _ => {
                   // alloc: a decoded stack of three or more, once at its length
                   let mut many = ThinVec::with_capacity(len);
                   for _ in 0..len {
                       many.push(next()?);
                   }
                   CaptureSourceStackRepr::Many(many)
               }
           }))
       }
   ```

5. Imports: `FxIndexMap` leaves `use base_db::{Db, Crate, FxIndexMap};` and `use
   smallvec::SmallVec;` goes if nothing else uses them (today only `ClosureData` and
   `CaptureInfo` do); `Hasher` leaves `hash::{Hash, Hasher}` if unused.

6. Test for `MinCaptures` (in `infer.rs`'s test module, replacing the `hash_unordered` ones):

   ```rust
   #[test]
   fn min_captures_split_their_block_by_root() {
       let types = &crate::next_solver::default_types().types;
       let capture = |var: u32| CapturedPlace {
           place: Place {
               base_ty: types.i32.store(),
               base: PlaceBase::Upvar { closure: la_arena::Idx::from_raw(0.into()), var_id: la_arena::Idx::from_raw(var.into()) },
               projections: Box::default(),
           },
           info: CaptureInfo { sources: Box::default(), capture_kind: UpvarCapture::ByValue },
           mutability: Mutability::Not,
       };
       let binding = |raw: u32| BindingId::from_raw(raw.into());
       let min = MinCaptures::from_parts(
           Box::new([(binding(4), 2), (binding(1), 2), (binding(7), 3)]),
           Box::new([capture(4), capture(4), capture(7)]),
       );
       let roots: Vec<_> = min.iter().map(|(binding, captures)| (binding, captures.len())).collect();
       assert_eq!(roots, [(binding(4), 2), (binding(1), 0), (binding(7), 1)]);
       assert_eq!(min.values().len(), 3);
       assert_eq!(min.flattened().len(), 3);
       let swapped = MinCaptures::from_parts(
           Box::new([(binding(1), 0), (binding(4), 2), (binding(7), 3)]),
           Box::new([capture(4), capture(4), capture(7)]),
       );
       assert_ne!(min, swapped, "the order of the roots is the order of the upvars");
   }
   ```

   (`default_types` is what `persist.rs`'s tests use; adjust the `Idx` paths to the test
   module's imports.)

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-infer-a-ctx (21:36): items 1 to 6 in `infer.rs` as written (imports: `Hasher`, `FxIndexMap`, `SmallVec` gone; the test sits in `mod tests` with `use super::*;`, its `Idx` paths `la_arena::Idx::from_raw`).

### X3. `resolve_all` (mem-db-infer-ctx, `infer.rs` about line 2149)

Before:

```rust
        for closure_data in closures_data.values_mut() {
            let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = closure_data;
            let dummy_place = || Place {
                base_ty: types.types.error.store(),
                base: closure::analysis::expr_use_visitor::PlaceBase::Rvalue,
                projections: Vec::new(),
            };
            if let Some((_, place)) = kind_origin {
                resolver.resolve_completely_with_default(place, dummy_place());
                place.projections.shrink_to_fit();
            }

            for (place, _, sources) in fake_reads {
                resolver.resolve_completely_with_default(place, dummy_place());
                place.projections.shrink_to_fit();
                for source in &mut *sources {
                    source.shrink_to_fit();
                }
                sources.shrink_to_fit();
            }

            for min_capture in min_captures.values_mut() {
                for captured in &mut *min_capture {
                    let CapturedPlace { place, info, mutability: _ } = captured;
                    resolver.resolve_completely_with_default(place, dummy_place());
                    let CaptureInfo { sources, capture_kind: _ } = info;
                    for source in &mut *sources {
                        source.shrink_to_fit();
                    }
                    sources.shrink_to_fit();
                }
                min_capture.shrink_to_fit();
            }
            min_captures.shrink_to_fit();

            resolver.resolve_completely(liberated_sig);
```

After (same order of resolution, so the same diagnostics in the same order; nothing is shrunk
because everything is built at its size; the fold of a `Box<[Projection]>` goes through `Vec`'s
in-place fold and allocates nothing):

```rust
        for closure_data in closures_data.values_mut() {
            let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = closure_data;
            let dummy_place = || Place {
                base_ty: types.types.error.store(),
                base: closure::analysis::expr_use_visitor::PlaceBase::Rvalue,
                projections: Box::default(),
            };
            if let Some((_, place)) = kind_origin {
                resolver.resolve_completely_with_default(place, dummy_place());
            }
            for (place, _, _) in fake_reads.iter_mut() {
                resolver.resolve_completely_with_default(place, dummy_place());
            }
            for CapturedPlace { place, .. } in min_captures.flattened_mut() {
                resolver.resolve_completely_with_default(place, dummy_place());
            }

            resolver.resolve_completely(liberated_sig);
```

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-infer-a-ctx (21:36): as written, after ctx A8 over the same loop (mem-db-infer.md section 6 item 2).

### X4. Persist (mem-db-infer-ctx, `infer/persist.rs`)

The format does not change (no version bump): same words in the same order.

1. Encoder `fn closure` (about line 1857): before `for (&binding, captures) in min_captures {`;
   after `for (binding, captures) in min_captures {`. `self.sources(sources)` and the fake read
   loop compile as they are (`&Box<[CaptureSourceStack]>` coerces to `&[CaptureSourceStack]`).

2. Decoder `fn place` (about line 2484): before

   ```rust
           let len = self.len()?;
           let projections = (0..len)
               .map(|_| {
                   let ty = self.ty(tys)?;
                   let kind = match self.word()? {
                       ..
                       _ => return Err(self.invalid("a projection of no kind")),
                   };
                   Ok(Projection { ty, kind })
               })
               .collect::<Result<_, FormatError>>()?;
           Ok(Place { base_ty, base, projections })
   ```

   after (`len()` is bounded by the words left, so the capacity is safe):

   ```rust
           let len = self.len()?;
           // alloc: the decoded place's projections, once at their number
           let mut projections = Vec::with_capacity(len);
           for _ in 0..len {
               let ty = self.ty(tys)?;
               let kind = match self.word()? {
                   ..
                   _ => return Err(self.invalid("a projection of no kind")),
               };
               projections.push(Projection { ty, kind });
           }
           Ok(Place { base_ty, base, projections: projections.into_boxed_slice() })
   ```

3. Decoder `fn sources` (about line 2502): before

   ```rust
       fn sources(&mut self) -> Result<SmallVec<[CaptureSourceStack; 2]>, FormatError> {
           let len = self.len()?;
           (0..len)
               .map(|_| {
                   let len = self.len()?;
                   if len == 0 {
                       return Err(self.invalid("a capture source stack without a source"));
                   }
                   let mut stack = CaptureSourceStack::from_single(self.node()?);
                   for _ in 1..len {
                       stack.push(self.node()?);
                   }
                   Ok(stack)
               })
               .collect()
       }
   ```

   after:

   ```rust
       fn sources(&mut self) -> Result<Box<[CaptureSourceStack]>, FormatError> {
           let len = self.len()?;
           // alloc: the decoded sources, once at their number
           let mut sources = Vec::with_capacity(len);
           for _ in 0..len {
               let len = self.len()?;
               if len == 0 {
                   return Err(self.invalid("a capture source stack without a source"));
               }
               sources.push(CaptureSourceStack::try_from_fn(len, || self.node())?);
           }
           Ok(sources.into_boxed_slice())
       }
   ```

4. Decoder `fn closure` (about line 2519): the number of captures is known only after reading
   every root, so they gather in the frame and are copied once into the result. It takes the
   decode's frame; its one caller passes it (from `frame::current` inside the query that decodes,
   or the `frame::task(Tag::Infer, ..)` of a decode entry outside one, R1). Before:

   ```rust
       fn closure(&mut self, tys: &impl TyDecode) -> Result<ClosureData, FormatError> {
           let len = self.len()?;
           let mut min_captures = crate::infer::RootVariableMinCaptureList::default();
           for _ in 0..len {
               let binding = self.local()?;
               let len = self.len()?;
               let captures = (0..len)
                   .map(|_| {
                       let place = self.place(tys)?;
                       let sources = self.sources()?;
                       let capture_kind = match self.word()? { .. };
                       let info = CaptureInfo { sources, capture_kind };
                       Ok(CapturedPlace { place, info, mutability: self.mutability()? })
                   })
                   .collect::<Result<_, FormatError>>()?;
               if min_captures.insert(binding, captures).is_some() {
                   return Err(self.invalid("a closure that captures a variable twice"));
               }
           }
           let len = self.len()?;
           let fake_reads = (0..len)
               .map(|_| {
                   let place = self.place(tys)?;
                   let cause = match self.word()? { .. };
                   Ok((place, cause, self.sources()?))
               })
               .collect::<Result<_, FormatError>>()?;
   ```

   After (the `..` matches are unchanged):

   ```rust
       fn closure(&mut self, tys: &impl TyDecode, frame: &Frame) -> Result<ClosureData, FormatError> {
           let len = self.len()?;
           // alloc: the decoded roots, once at their number
           let mut roots = Vec::with_capacity(len);
           let mut seen = hashbrown::HashSet::with_capacity_and_hasher_in(len, FxBuildHasher, frame);
           let mut captures = Vec::new_in(frame);
           for _ in 0..len {
               let binding = self.local()?;
               if !seen.insert(binding) {
                   return Err(self.invalid("a closure that captures a variable twice"));
               }
               let len = self.len()?;
               captures.reserve(len);
               for _ in 0..len {
                   let place = self.place(tys)?;
                   let sources = self.sources()?;
                   let capture_kind = match self.word()? { .. };
                   let info = CaptureInfo { sources, capture_kind };
                   captures.push(CapturedPlace { place, info, mutability: self.mutability()? });
               }
               let end = u32::try_from(captures.len())
                   .map_err(|_| self.invalid("a closure with 2^32 captures or more"))?;
               roots.push((binding, end));
           }
           // alloc: the decoded captures, once at their number (a drain is `TrustedLen`)
           let min_captures = MinCaptures::from_parts(roots.into_boxed_slice(), captures.drain(..).collect());
           let len = self.len()?;
           // alloc: the decoded fake reads, once at their number
           let mut fake_reads = Vec::with_capacity(len);
           for _ in 0..len {
               let place = self.place(tys)?;
               let cause = match self.word()? { .. };
               fake_reads.push((place, cause, self.sources()?));
           }
           let fake_reads = fake_reads.into_boxed_slice();
   ```

   The duplicate check now runs before the captures are read rather than after; the error and
   its `at` (the record's root) are the same. `persist.rs` imports `blaze_mem::Frame`,
   `rustc_hash::FxBuildHasher` and `crate::infer::MinCaptures` for it.

5. Tests (about lines 2666 to 3015). `fn place`: before `projections: vec![ .. ],`; after
   `projections: Box::new([ .. ]),`. `fn sources`: before

   ```rust
       fn sources(lens: &[u32]) -> SmallVec<[CaptureSourceStack; 2]> {
           lens.iter()
               .map(|&len| {
                   let mut stack = CaptureSourceStack::from_single(expr(1).into());
                   (1..len).for_each(|it| stack.push(if it % 2 == 0 { expr(it).into() } else { pat(it).into() }));
                   stack
               })
               .collect()
       }
   ```

   after:

   ```rust
       fn sources(lens: &[u32]) -> Box<[CaptureSourceStack]> {
           lens.iter()
               .map(|&len| {
                   let ids: Vec<ExprOrPatIdPacked> = std::iter::once(expr(1).into())
                       .chain((1..len).map(|it| if it % 2 == 0 { expr(it).into() } else { pat(it).into() }))
                       .collect();
                   CaptureSourceStack::from_slice(&ids)
               })
               .collect()
       }
   ```

   `fn closure`: before

   ```rust
           let mut min_captures = crate::infer::RootVariableMinCaptureList::default();
           min_captures.insert(
               binding(4),
               vec![
                   CapturedPlace { .. },
                   CapturedPlace { .. },
               ],
           );
           min_captures.insert(binding(1), Vec::new());
   ```

   after (the two `CapturedPlace`s unchanged; `binding(1)` keeps its empty group, so the
   round-trip still covers one):

   ```rust
           let min_captures = crate::infer::MinCaptures::from_parts(
               Box::new([(binding(4), 2), (binding(1), 2)]),
               Box::new([
                   CapturedPlace { .. },
                   CapturedPlace { .. },
               ]),
           );
   ```

   The round-trip test now also checks the order of the roots, since `Eq` is order-sensitive.

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-infer-a-persist (21:58): X4 1 to 5 as written in `infer/persist.rs` (`closure(tys, frame)`
> called with `decode_in`'s frame); `smallvec::SmallVec` import dropped (no other use left). Compiles
> only once `infer.rs`'s X2 (`MinCaptures`, `from_slice`, `try_from_fn`, `Box<[..]>` fields) is in.

### X5. Re-export (mem-db-infer, `crates/hir-ty/src/lib.rs:126-132`)

Before `CaptureInfo, CaptureSourceStack, CapturedPlace, ClosureData, UpvarCapture,`; after
`CaptureInfo, CaptureSourceStack, CapturedPlace, ClosureData, MinCaptures, MinCapturesIter,
UpvarCapture,` (so the public field's type can be named outside).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a (21:26): as written (`MinCaptures`, `MinCapturesIter` come from closure X2 in `infer.rs`, db-mem-infer-a-ctx).

## `closure.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): C0-C2 applied below by db-mem-infer-a-closure.

### C0. Impl header (line 51)

Before `impl<'db> InferenceContext<'db> {`; after `impl<'db, 'f> InferenceContext<'db, 'f> {`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:24): as written.

### C1. A new closure's data (lines 306-315, and the import on line 12)

Before:

```rust
        self.result.closures_data.insert(
            closure_expr,
            ClosureData {
                liberated_sig: StoredFnSig::new(liberated_sig),
                fake_reads: Box::default(),
                min_captures: IndexMap::default(),
                kind_origin: None,
            },
        );
```

After: `min_captures: Default::default(),` (no allocation, as before), and line 12 `use
indexmap::IndexMap;` is deleted. Analysis fills `min_captures` once (A4); `walk_captures`
removes and reinserts entries of `closures_data`, which allocates nothing.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:24): as written; also expr X4's `CoerceMany::new(liberated_sig.output(), self.frame)` at the return coercion.

### C2. Supplied closure signature (lines 1020-1041)

Before:

```rust
        let (supplied, bound_vars) =
            self.with_ty_lowering(|ctx| ctx.lower_closure_params(decl_inputs));
        let supplied_arguments: Vec<_> = supplied
            .into_iter()
            .map(|input| match input {
                // The solver takes no type with a lifetime bound outside it.
                Some(ty) if ty.has_escaping_bound_vars() => self.table.insert_type_vars(ty),
                Some(ty) => self.process_user_written_ty(ty),
                None => self.table.next_ty_var(closure_expr.into()),
            })
            .collect();

        Binder::bind_with_vars(
            interner.mk_fn_sig(
                supplied_arguments,
                supplied_return,
```

After (`mk_fn_sig` takes any `IntoIterator<Item = Ty>` and interns from it; `interner` is a
`Copy` local, so the closure's borrow of `self` does not conflict; the type variables are made in
the same order, after `supplied_return`'s, as before):

```rust
        let (supplied, bound_vars) =
            self.with_ty_lowering(|ctx| ctx.lower_closure_params(decl_inputs));
        let supplied_arguments = supplied.into_iter().map(|input| match input {
            // The solver takes no type with a lifetime bound outside it.
            Some(ty) if ty.has_escaping_bound_vars() => self.table.insert_type_vars(ty),
            Some(ty) => self.process_user_written_ty(ty),
            None => self.table.next_ty_var(closure_expr.into()),
        });

        Binder::bind_with_vars(
            interner.mk_fn_sig(
                supplied_arguments,
                supplied_return,
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:24): in the combined form of `mem-db-infer.md` 6.1 (with lower 22: `supplied` is a frame `Vec` the call fills, `bound_vars` its return).

### What `closure.rs` still allocates, and whose it is

Signature deduction has no per-closure lists of its own: the obligations and bounds it reads come
from other parts.

| site | what | owner, and what to do |
|---|---|---|
| lines 413, 678, 1080 `self.table.obligations_for_self_ty(vid, UseSubtyping::No)` | `SmallVec<[PredicateObligation; 4]>` per closure with an inferred expected type; spills (1 global) on a 5th pending obligation of the variable, which real code reaches (`F: Fn(..) + Send + Clone + 'static` with its projections) | mem-db-infer-ctx (`unify.rs:212`): return a frame `Vec` or take a visitor; both callers only iterate |
| line 440 `rustc_type_ir::elaborate::elaborate(..)` | the `Elaborator`'s stack `Vec` and visited set (global), per closure deduced from predicates | vendor `rustc_type_ir`, mem-db-solver |
| line 923 `PredicateObligations::new()` | the solver's obligation list for the supplied-vs-expected check | mem-db-solver (its type) |
| line 1021 `lower_closure_params` | returns `Vec<Option<Ty>>` | mem-db-lower (`lower.rs:1110`) |
| C2 `mk_fn_sig` | `Tys::new_from_iter` collects into `SmallVec<[Ty; 8]>` before interning (spills above 7 parameters) | next_solver interner, mem-db-solver |
| line 311 `Box::default()`, C1 `Default::default()` | empty results | none: no allocation |

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): notes, each row settled by its owner: `obligations_for_self_ty` stays (mem-db-infer-ctx.md U3, crates/hir-ty/src/infer/unify.rs:215), `lower_closure_params` fills the caller's `Extend` (crates/hir-ty/src/lower.rs:1195), the solver rows are mem-db-solver's.

## `closure/analysis/expr_use_visitor.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): E1-E8 applied below by db-mem-infer-a-closure.

### E1. Imports (lines 7-35)

Add `use std::mem;`, `use blaze_mem::Frame;` and `use rustc_type_ir::{FallibleTypeFolder,
TypeFoldable, TypeFolder, TypeVisitable, TypeVisitor};` (next to the existing `rustc_type_ir`
line), and `DbInterner` to the `next_solver::{..}` import. Before `use smallvec::{SmallVec,
smallvec};`; after `use smallvec::SmallVec;`. `CaptureSourceStack` stays imported (`Sources::of`,
`SourceStack::to_result`).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written (`rustc_type_ir` traits on a second `use` line next to the `inherent` one).

### E2. Types (lines 74-183)

`Projection` (line 74): before `#[derive(Clone, Debug, PartialEq, Eq, Hash, TypeVisitable,
TypeFoldable)]`; after `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, TypeVisitable,
TypeFoldable)]` (a 4-byte `StoredTy` and a `Copy` kind).

`Place` (lines 88-131): before `pub projections: Vec<Projection>,`; after `pub projections:
Box<[Projection]>,`, and its three methods delegate to a view shared with `PlaceBuf`. Before:

```rust
impl Place {
    /// Returns an iterator of the types that have to be dereferenced to access
    /// the `Place`.
    /// ..
    pub fn deref_tys<'db>(&self) -> impl Iterator<Item = Ty<'db>> {
        self.projections.iter().enumerate().rev().filter_map(move |(index, proj)| {
            if ProjectionKind::Deref == proj.kind {
                Some(self.ty_before_projection(index))
            } else {
                None
            }
        })
    }

    /// Returns the type of this `Place` after all projections have been applied.
    pub fn ty<'db>(&self) -> Ty<'db> {
        self.projections.last().map_or(self.base_ty.as_ref(), |proj| proj.ty.as_ref())
    }

    /// Returns the type of this `Place` immediately before `projection_index`th projection
    /// is applied.
    pub fn ty_before_projection<'db>(&self, projection_index: usize) -> Ty<'db> {
        assert!(projection_index < self.projections.len());
        if projection_index == 0 {
            self.base_ty.as_ref()
        } else {
            self.projections[projection_index - 1].ty.as_ref()
        }
    }
}
```

After (doc comments kept on the `Place` methods):

```rust
/// A place's base type and projections, as [`Place`] and [`PlaceBuf`] both hold them.
#[derive(Clone, Copy)]
struct PlaceView<'a> {
    base_ty: StoredTy,
    projections: &'a [Projection],
}

impl<'a> PlaceView<'a> {
    fn deref_tys<'db>(self) -> impl Iterator<Item = Ty<'db>> {
        self.projections.iter().enumerate().rev().filter_map(move |(index, proj)| {
            if ProjectionKind::Deref == proj.kind {
                Some(self.ty_before_projection(index))
            } else {
                None
            }
        })
    }

    fn ty<'db>(self) -> Ty<'db> {
        self.projections.last().map_or(self.base_ty.as_ref(), |proj| proj.ty.as_ref())
    }

    fn ty_before_projection<'db>(self, projection_index: usize) -> Ty<'db> {
        assert!(projection_index < self.projections.len());
        if projection_index == 0 {
            self.base_ty.as_ref()
        } else {
            self.projections[projection_index - 1].ty.as_ref()
        }
    }
}

impl Place {
    fn view(&self) -> PlaceView<'_> {
        PlaceView { base_ty: self.base_ty, projections: &self.projections }
    }

    pub fn deref_tys<'db>(&self) -> impl Iterator<Item = Ty<'db>> {
        self.view().deref_tys()
    }

    pub fn ty<'db>(&self) -> Ty<'db> {
        self.view().ty()
    }

    pub fn ty_before_projection<'db>(&self, projection_index: usize) -> Ty<'db> {
        self.view().ty_before_projection(projection_index)
    }
}

/// A [`Place`] while closures are analysed: its projections are in the frame.
#[derive(Clone, Debug)]
pub(crate) struct PlaceBuf<'f> {
    pub(crate) base_ty: StoredTy,
    pub(crate) base: PlaceBase,
    pub(crate) projections: Vec<Projection, &'f Frame>,
}

impl<'f> PlaceBuf<'f> {
    pub(crate) fn new(base_ty: Ty<'_>, base: PlaceBase, frame: &'f Frame) -> Self {
        PlaceBuf { base_ty: base_ty.store(), base, projections: Vec::new_in(frame) }
    }

    /// A result's place, for analysis to extend.
    pub(crate) fn of(place: &Place, frame: &'f Frame) -> Self {
        PlaceBuf { base_ty: place.base_ty, base: place.base, projections: place.projections.to_vec_in(frame) }
    }

    /// The result's place, at its length.
    pub(crate) fn to_place(&self) -> Place {
        // alloc: a result's projections, once at their number (none for a place without any)
        Place { base_ty: self.base_ty, base: self.base, projections: Box::from(&*self.projections) }
    }

    fn view(&self) -> PlaceView<'_> {
        PlaceView { base_ty: self.base_ty, projections: &self.projections }
    }

    pub(crate) fn deref_tys<'db>(&self) -> impl Iterator<Item = Ty<'db>> {
        self.view().deref_tys()
    }

    pub(crate) fn ty<'db>(&self) -> Ty<'db> {
        self.view().ty()
    }

    pub(crate) fn ty_before_projection<'db>(&self, projection_index: usize) -> Ty<'db> {
        self.view().ty_before_projection(projection_index)
    }
}

impl<'db> TypeVisitable<DbInterner<'db>> for PlaceBuf<'_> {
    fn visit_with<V: TypeVisitor<DbInterner<'db>>>(&self, visitor: &mut V) -> V::Result {
        rustc_ast_ir::try_visit!(self.base_ty.visit_with(visitor));
        self.projections[..].visit_with(visitor)
    }
}

/// Folds the types in place: the frame block is kept.
impl<'db> TypeFoldable<DbInterner<'db>> for PlaceBuf<'_> {
    fn try_fold_with<F: FallibleTypeFolder<DbInterner<'db>>>(
        mut self,
        folder: &mut F,
    ) -> std::result::Result<Self, F::Error> {
        self.base_ty = self.base_ty.try_fold_with(folder)?;
        for projection in &mut self.projections {
            *projection = projection.try_fold_with(folder)?;
        }
        Ok(self)
    }

    fn fold_with<F: TypeFolder<DbInterner<'db>>>(mut self, folder: &mut F) -> Self {
        self.base_ty = self.base_ty.fold_with(folder);
        for projection in &mut self.projections {
            *projection = projection.fold_with(folder);
        }
        self
    }
}

/// A source stack while closures are analysed: base first, one id per projection after it.
#[derive(Clone, Debug)]
pub(crate) enum SourceStack<'f> {
    One(ExprOrPatIdPacked),
    Two([ExprOrPatIdPacked; 2]),
    Many(Vec<ExprOrPatIdPacked, &'f Frame>),
}

impl<'f> SourceStack<'f> {
    pub(crate) fn of(stack: &CaptureSourceStack, frame: &'f Frame) -> Self {
        match **stack {
            [one] => Self::One(one),
            [first, second] => Self::Two([first, second]),
            ref many => Self::Many(many.to_vec_in(frame)),
        }
    }

    pub(crate) fn as_slice(&self) -> &[ExprOrPatIdPacked] {
        match self {
            Self::One(it) => std::slice::from_ref(it),
            Self::Two(it) => it,
            Self::Many(it) => it,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub(crate) fn final_source(&self) -> ExprOrPatIdPacked {
        *self.as_slice().last().expect("should always have a final source")
    }

    fn push(&mut self, id: ExprOrPatIdPacked, frame: &'f Frame) {
        match self {
            Self::One(first) => *self = Self::Two([*first, id]),
            Self::Two([first, second]) => {
                let mut ids = Vec::with_capacity_in(4, frame);
                ids.extend([*first, *second, id]);
                *self = Self::Many(ids);
            }
            Self::Many(ids) => ids.push(id),
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        debug_assert!(len > 0);
        match self {
            Self::One(_) => {}
            Self::Two([first, _]) => {
                if len == 1 {
                    *self = Self::One(*first)
                }
            }
            Self::Many(ids) => ids.truncate(len),
        }
    }

    pub(crate) fn to_result(&self) -> CaptureSourceStack {
        CaptureSourceStack::from_slice(self.as_slice())
    }
}

/// The source stacks of one place while closures are analysed: almost always one.
#[derive(Clone, Debug)]
pub(crate) enum Sources<'f> {
    One(SourceStack<'f>),
    Many(Vec<SourceStack<'f>, &'f Frame>),
}

impl<'f> Sources<'f> {
    pub(crate) fn one(stack: SourceStack<'f>) -> Self {
        Self::One(stack)
    }

    pub(crate) fn none(frame: &'f Frame) -> Self {
        Self::Many(Vec::new_in(frame))
    }

    pub(crate) fn of(sources: &[CaptureSourceStack], frame: &'f Frame) -> Self {
        match sources {
            [one] => Self::One(SourceStack::of(one, frame)),
            _ => {
                let mut all = Vec::with_capacity_in(sources.len(), frame);
                all.extend(sources.iter().map(|it| SourceStack::of(it, frame)));
                Self::Many(all)
            }
        }
    }

    pub(crate) fn as_slice(&self) -> &[SourceStack<'f>] {
        match self {
            Self::One(it) => std::slice::from_ref(it),
            Self::Many(it) => it,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [SourceStack<'f>] {
        match self {
            Self::One(it) => std::slice::from_mut(it),
            Self::Many(it) => it,
        }
    }

    pub(crate) fn first(&self) -> Option<&SourceStack<'f>> {
        self.as_slice().first()
    }

    pub(crate) fn push(&mut self, stack: SourceStack<'f>, frame: &'f Frame) {
        match self {
            Self::Many(all) => all.push(stack),
            Self::One(_) => {
                let mut all = Vec::with_capacity_in(2, frame);
                all.extend(mem::replace(self, Self::none(frame)));
                all.push(stack);
                *self = Self::Many(all);
            }
        }
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&SourceStack<'f>) -> bool, frame: &'f Frame) {
        match self {
            Self::One(it) => {
                if !keep(it) {
                    *self = Self::none(frame);
                }
            }
            Self::Many(all) => all.retain(keep),
        }
    }

    /// The result's sources, at their number.
    pub(crate) fn to_result(&self) -> Box<[CaptureSourceStack]> {
        // alloc: a result's sources, once at their number (a slice map is `TrustedLen`)
        self.as_slice().iter().map(SourceStack::to_result).collect()
    }
}

impl<'f> IntoIterator for Sources<'f> {
    type Item = SourceStack<'f>;
    type IntoIter = Either<std::iter::Once<SourceStack<'f>>, std::vec::IntoIter<SourceStack<'f>, &'f Frame>>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::One(it) => Either::Left(std::iter::once(it)),
            Self::Many(all) => Either::Right(all.into_iter()),
        }
    }
}
```

(`mem` is `std::mem`; add it to the imports.)

`PlaceWithOrigin` (lines 133-183): before

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PlaceWithOrigin {
    /// `ExprId`s or `PatId`s of the expressions or patterns producing this value.
    pub origins: SmallVec<[CaptureSourceStack; 2]>,

    /// Information about the `Place`.
    pub place: Place,
}

impl PlaceWithOrigin {
    fn new_no_projections<'db>(
        origin: impl Into<ExprOrPatIdPacked>,
        base_ty: Ty<'db>,
        base: PlaceBase,
    ) -> PlaceWithOrigin {
        Self::new(
            smallvec![CaptureSourceStack::from_single(origin.into())],
            base_ty,
            base,
            Vec::new(),
        )
    }

    fn new<'db>(
        origins: SmallVec<[CaptureSourceStack; 2]>,
        base_ty: Ty<'db>,
        base: PlaceBase,
        projections: Vec<Projection>,
    ) -> PlaceWithOrigin {
        debug_assert!(origins.iter().all(|origin| origin.len() == projections.len() + 1));
        PlaceWithOrigin { origins, place: Place { base_ty: base_ty.store(), base, projections } }
    }

    fn push_projection(&mut self, projection: Projection, origin: ExprOrPatIdPacked) {
        self.place.projections.push(projection);
        for origin_stack in &mut self.origins {
            origin_stack.push(origin);
        }
    }

    pub(crate) fn span(&self) -> Span {
        match self.origins.first() {
```

after (nothing compared or hashed `PlaceWithOrigin`, so only `Clone` and `Debug` stay):

```rust
#[derive(Clone, Debug)]
pub(crate) struct PlaceWithOrigin<'f> {
    /// `ExprId`s or `PatId`s of the expressions or patterns producing this value.
    pub origins: Sources<'f>,

    /// Information about the `Place`.
    pub place: PlaceBuf<'f>,
}

impl<'f> PlaceWithOrigin<'f> {
    fn new_no_projections(
        origin: impl Into<ExprOrPatIdPacked>,
        base_ty: Ty<'_>,
        base: PlaceBase,
        frame: &'f Frame,
    ) -> Self {
        PlaceWithOrigin {
            origins: Sources::one(SourceStack::One(origin.into())),
            place: PlaceBuf::new(base_ty, base, frame),
        }
    }

    /// A result's capture, as the enclosing closure's place of `base`.
    fn of_capture(place: &Place, base: PlaceBase, sources: &[CaptureSourceStack], frame: &'f Frame) -> Self {
        let place = PlaceBuf { base_ty: place.base_ty, base, projections: place.projections.to_vec_in(frame) };
        let origins = Sources::of(sources, frame);
        debug_assert!(origins.as_slice().iter().all(|origin| origin.len() == place.projections.len() + 1));
        PlaceWithOrigin { origins, place }
    }

    fn push_projection(&mut self, projection: Projection, origin: ExprOrPatIdPacked) {
        let frame = *self.place.projections.allocator();
        self.place.projections.push(projection);
        for origin_stack in self.origins.as_mut_slice() {
            origin_stack.push(origin, frame);
        }
    }

    pub(crate) fn span(&self) -> Span {
        match self.origins.first() {
```

(the rest of `span` unchanged). The first `push_projection` on a place takes one frame block of
4 projections (64 bytes); deeper places grow it in place while it is the frame's top.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written; `Place`'s method docs kept; `PlaceWithOrigin::new` deleted (its one caller is E7's `of_capture`).

### E3. `Delegate` and the visitor's lifetimes (lines 293-428, 1158)

Mechanical: `pub(crate) trait Delegate<'db>` becomes `pub(crate) trait Delegate<'db, 'f>`; in
it and in `impl<'db, D: Delegate<'db>> Delegate<'db> for &mut D` (which becomes `impl<'db, 'f,
D: Delegate<'db, 'f>> Delegate<'db, 'f> for &mut D`) every `PlaceWithOrigin` becomes
`PlaceWithOrigin<'f>` and every `&mut InferenceContext<'db>` becomes `&mut
InferenceContext<'db, 'f>`. The visitor:

```rust
pub(crate) struct ExprUseVisitor<'a, 'db, 'f, D: Delegate<'db, 'f>> {
    cx: &'a mut InferenceContext<'db, 'f>,
    delegate: D,
    closure_expr: ExprId,
    upvars: UpvarsRef<'db>,
}

impl<'a, 'db, 'f, D: Delegate<'db, 'f>> ExprUseVisitor<'a, 'db, 'f, D> {
    pub(crate) fn new(
        cx: &'a mut InferenceContext<'db, 'f>,
```

and line 1158 `impl<'db, D: Delegate<'db>> ExprUseVisitor<'_, 'db, D> {` becomes `impl<'db, 'f,
D: Delegate<'db, 'f>> ExprUseVisitor<'_, 'db, 'f, D> {`. In both impls every `PlaceWithOrigin` in
a type (parameters, `Result<PlaceWithOrigin>`, the `F: FnMut(&mut Self, PlaceWithOrigin,
CatPatternPat)` bound of `cat_pattern`, the `walk_deref_pat` closure's parameter) becomes
`PlaceWithOrigin<'f>`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written, which also covers ctx A0's `expr_use_visitor.rs` lines (the `Delegate` methods take `InferenceContext<'db, 'f>`, not `'_`, since they build frame values from `ctx.frame`).

### E4. Constructors take the frame (lines 1367-1370, 1396, 1414-1418, 1421-1423)

Each `PlaceWithOrigin::new_no_projections(a, b, c)` gains `, self.cx.frame` as its last
argument: the static item (line 1367), `cat_local` (line 1396), `cat_upvar` (line 1414) and
`cat_rvalue` (line 1422). None of them allocates: the projections are empty and the one source
is inline.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written, all four.

### E5. `match`: the last arm takes the place (lines 566-574)

Before:

```rust
            Expr::Match { expr: discr, arms } => {
                let discr_place = self.cat_expr(discr)?;
                self.fake_read_scrutinee(discr_place.clone(), true);
                self.walk_expr(discr)?;

                for arm in arms {
                    self.walk_arm(discr_place.clone(), arm)?;
                }
            }
```

After (same arms in the same order):

```rust
            Expr::Match { expr: discr, arms } => {
                let discr_place = self.cat_expr(discr)?;
                self.fake_read_scrutinee(discr_place.clone(), true);
                self.walk_expr(discr)?;

                if let Some((last, arms)) = arms.split_last() {
                    for arm in arms {
                        self.walk_arm(discr_place.clone(), arm)?;
                    }
                    self.walk_arm(discr_place, last)?;
                }
            }
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written.

### E6. `walk_pat`: no copy of the scrutinee, discriminant reads copy only when they need to (lines 879-1029)

Line 880, before `self.cat_pattern(discr_place.clone(), pat, &mut |this, place, pat| {`; after
`self.cat_pattern(discr_place, pat, &mut |this, place, pat| {` (`discr_place` is not used after
it).

Lines 907-912, before (a copy of every pattern's place, used by few):

```rust
            let read_discriminant = {
                let place = place.clone();
                |this: &mut Self| {
                    this.delegate.borrow(place, BorrowKind::Immutable, this.cx);
                }
            };
```

after:

```rust
            let read_discriminant = |this: &mut Self, place: PlaceWithOrigin<'f>| {
                this.delegate.borrow(place, BorrowKind::Immutable, this.cx);
            };
```

and its calls: line 932 (a binding with a guard, whose place is used after) `read_discriminant(this,
place.clone());`; lines 972, 976 (`Pat::Path`), 989 (`Pat::Lit | Pat::Range`), 993
(`Pat::Record | Pat::TupleStruct`) and 1005 (`Pat::Slice`) `read_discriminant(this, place);`
(each arm reads `place.place.ty()` in its condition before the call, and uses `place` no more).
The order of delegate calls is unchanged.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written, all six calls.

### E7. `walk_captures`: a nested closure's result becomes frame values (lines 1060-1134)

Before:

```rust
                self.delegate.fake_read(
                    PlaceWithOrigin { place: fake_read.clone(), origins: origins.clone() },
                    *cause,
                    self.cx,
                );
            }

            for (var_id, min_list) in closure_data.min_captures.iter() {
                if !self.upvars.contains(*var_id) {
                    ..
                    continue;
                }
                for captured_place in min_list {
                    let place = &captured_place.place;
                    let capture_info = &captured_place.info;

                    // Mark the place to be captured by the enclosing closure
                    let place_base =
                        PlaceBase::Upvar { var_id: *var_id, closure: self.closure_expr };
                    let place_with_id = PlaceWithOrigin::new(
                        capture_info.sources.clone(),
                        place.base_ty.as_ref(),
                        place_base,
                        place.projections.clone(),
                    );
```

After:

```rust
                let frame = self.cx.frame;
                self.delegate.fake_read(
                    PlaceWithOrigin {
                        place: PlaceBuf::of(fake_read, frame),
                        origins: Sources::of(origins, frame),
                    },
                    *cause,
                    self.cx,
                );
            }

            for (var_id, min_list) in closure_data.min_captures.iter() {
                if !self.upvars.contains(var_id) {
                    ..
                    continue;
                }
                for captured_place in min_list {
                    let place = &captured_place.place;
                    let capture_info = &captured_place.info;

                    // Mark the place to be captured by the enclosing closure
                    let place_base = PlaceBase::Upvar { var_id, closure: self.closure_expr };
                    let place_with_id = PlaceWithOrigin::of_capture(
                        place,
                        place_base,
                        &capture_info.sources,
                        self.cx.frame,
                    );
```

The `remove` and `insert` of `closure_data` around it stay (borrowck; the map does not grow).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written.

### E8. `cat_pattern`: a leaf pattern takes the place (lines 1565-1574)

Before:

```rust
        op(self, place_with_id.clone(), pat.into())?;

        match self.cx.store.pat(pat) {
```

After (the leaf kinds are exactly the arm at lines 1693-1703 that does nothing after `op`):

```rust
        let leaf = matches!(
            self.cx.store.pat(pat),
            Pat::Bind { subpat: None, .. }
                | Pat::Expr(..)
                | Pat::Path(_)
                | Pat::Lit(..)
                | Pat::Range { .. }
                | Pat::Missing
                | Pat::Rest
                | Pat::NotNull
                | Pat::Wild
        );
        if leaf {
            return op(self, place_with_id, pat.into());
        }
        op(self, place_with_id.clone(), pat.into())?;

        match self.cx.store.pat(pat) {
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:48): as written; every place copy the file keeps (the table below) has a `// clone:` line for `hot_lint`.

### What `expr_use_visitor.rs` still allocates

| site | what | why it stays |
|---|---|---|
| `consume_closure_body` `param_place.clone()` (line 437), `walk_local` `expr_place.clone()` (line 734), E5's fake read and every arm but the last | frame copy of a place | the place is read twice (fake read, then the pattern); a parameter's place is an rvalue, so its copy allocates nothing |
| `walk_struct_expr` `with_place.clone()` (line 765) per field the literal leaves to the spread | frame copy | each field extends its own copy |
| `walk_adjustment` `place_with_id.clone()` (lines 811, 823, 827) | frame copy per adjustment that uses its input | the place goes on to the next adjustment |
| E6 `place.clone()` for a guarded binding | frame copy | the binding is borrowed and then moved or copied |
| `cat_pattern` `place_with_id.clone()` for an overloaded deref (line 1552), a non-leaf pattern (E8), each tuple, tuple-struct and record subpattern (lines 1587, 1606, 1626), each or-alternative (line 1639), each slice element (lines 1671, 1676, 1689) | frame copy | each subpattern extends its own copy |
| every copy above | 0 blocks for a place without projections and stacks of 2 or fewer; else 1 frame block for the projections, at their length, and 1 per stack of 3 or more | a `Vec` clone is exact |
| `push_projection` | 1 frame block (4 projections) per place that gets its first projection; a stack's third id takes 1 frame block of 4 | grows in place while the frame's top |
| E7 `PlaceBuf::of`, `Sources::of` | frame copies of a nested closure's result | analysis extends them; the result is immutable |
| `expr_adjustments`, `pat_adjustments` `SmallVec<[_; 5]>` (lines 789-797) | stack copy of an expression's adjustments | R3: it spills only above 5 adjustments on one expression, which takes 5 derefs and a borrow on one receiver; the copy exists because `cat_expr_` needs `&mut self` while reading them |

No `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`, `FxHashMap`, `FxHashSet`,
`ThinVec` or index map is left in this file; `PlaceBuf::to_place` and `Sources::to_result` are
the result's own allocations.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): notes: the copies E8's applier marked with `// clone:`; nothing to edit.

## `closure/analysis.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): A0-A8 applied below by db-mem-infer-a-closure.

### A0. Imports and the impl header (lines 33-71, 197)

Add `use blaze_mem::Frame;` and `use rustc_type_ir::{Interner as _, TypeVisitableExt as _};`
(for `renormalize_rigid_aliases` and `has_non_rigid_aliases`). Before `use rustc_hash::{FxBuildHasher,
FxHashMap, FxHashSet};`; after `use rustc_hash::{FxBuildHasher, FxHashMap};`. Delete `use
smallvec::{SmallVec, smallvec};`. In the `crate::infer` import, `CaptureSourceStack` goes if
unused; add `PlaceBuf, SourceStack, Sources` to the `expr_use_visitor::{..}` list. Line 197:
before `impl<'db> InferenceContext<'db> {`; after `impl<'db, 'f> InferenceContext<'db, 'f> {`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written; `CaptureSourceStack` and `Place` leave the import (unused), `MinCaptures` joins the `crate::infer` list (A1). Covers ctx A0's `analysis.rs` lines (`'f`, not `'_`, as E3).

### A1. Analysis-time capture types (lines 192-195, 1181-1213)

Before:

```rust
/// Intermediate format to store a captured `Place` and associated `CaptureInfo`
/// during capture analysis. Information in this map feeds into the minimum capture
/// analysis pass.
type InferredCaptureInformation = Vec<(Place, CaptureInfo)>;
```

After:

```rust
/// Intermediate format to store a captured `Place` and associated `CaptureInfo`
/// during capture analysis. Information in this map feeds into the minimum capture
/// analysis pass.
type InferredCaptureInformation<'f> = Vec<(PlaceBuf<'f>, CaptureInfoBuf<'f>), &'f Frame>;

/// A [`CaptureInfo`] while closures are analysed.
#[derive(Debug)]
struct CaptureInfoBuf<'f> {
    sources: Sources<'f>,
    capture_kind: UpvarCapture,
}

impl CaptureInfoBuf<'_> {
    fn to_result(&self) -> CaptureInfo {
        CaptureInfo { sources: self.sources.to_result(), capture_kind: self.capture_kind }
    }
}

/// A [`CapturedPlace`] while closures are analysed.
#[derive(Debug)]
struct CapturedPlaceBuf<'f> {
    place: PlaceBuf<'f>,
    info: CaptureInfoBuf<'f>,
    mutability: Mutability,
}

impl CapturedPlaceBuf<'_> {
    fn to_result(&self) -> CapturedPlace {
        CapturedPlace { place: self.place.to_place(), info: self.info.to_result(), mutability: self.mutability }
    }
}

/// The min captures of the closure being analysed, by root variable in first-capture order.
#[derive(Debug)]
struct MinCapturesBuf<'f> {
    roots: Vec<(BindingId, Vec<CapturedPlaceBuf<'f>, &'f Frame>), &'f Frame>,
    /// Where each root variable is in `roots`.
    index: hashbrown::HashMap<BindingId, u32, FxBuildHasher, &'f Frame>,
}

impl<'f> MinCapturesBuf<'f> {
    fn new(frame: &'f Frame) -> Self {
        MinCapturesBuf { roots: Vec::new_in(frame), index: hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame) }
    }

    /// The result, at its size.
    fn finish(&self) -> MinCaptures {
        let total = self.roots.iter().map(|(_, captures)| captures.len()).sum();
        // alloc: the result's captures, once at their number
        let mut captures = Vec::with_capacity(total);
        // alloc: the result's roots, once at their number (a slice map is `TrustedLen`)
        let roots = self
            .roots
            .iter()
            .map(|(binding, list)| {
                captures.extend(list.iter().map(CapturedPlaceBuf::to_result));
                (*binding, u32::try_from(captures.len()).expect("fewer than 2^32 captures"))
            })
            .collect();
        MinCaptures::from_parts(roots, captures.into_boxed_slice())
    }
}
```

`InferBorrowKind`, before:

```rust
struct InferBorrowKind {
    // The def-id of the closure whose kind and upvar accesses are being inferred.
    closure_def_id: ExprId,
    ..
    capture_information: InferredCaptureInformation,
    fake_reads: Vec<(Place, FakeReadCause, SmallVec<[CaptureSourceStack; 2]>)>,
}

impl<'db> euv::Delegate<'db> for InferBorrowKind {
```

after (doc comments kept):

```rust
struct InferBorrowKind<'f> {
    // The def-id of the closure whose kind and upvar accesses are being inferred.
    closure_def_id: ExprId,
    ..
    capture_information: InferredCaptureInformation<'f>,
    fake_reads: Vec<(PlaceBuf<'f>, FakeReadCause, Sources<'f>), &'f Frame>,
}

impl<'f> InferBorrowKind<'f> {
    fn new(closure_def_id: ExprId, frame: &'f Frame) -> Self {
        InferBorrowKind { closure_def_id, capture_information: Vec::new_in(frame), fake_reads: Vec::new_in(frame) }
    }
}

impl<'db, 'f> euv::Delegate<'db, 'f> for InferBorrowKind<'f> {
```

`MinCaptures` is imported from `crate::infer` (X2).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written (the roots' `alloc:` comment sits on the line above `.collect()` for `hot_lint`).

### A2. The delegate takes its places, not copies (lines 1215-1306)

Each method's parameters become `place_with_id: PlaceWithOrigin<'f>` and `ctx: &mut
InferenceContext<'db, 'f>`. `fake_read`, before:

```rust
        let mut dummy_capture_info =
            CaptureInfo { sources: SmallVec::new(), capture_kind: dummy_capture_kind };

        let place = ctx.normalize_capture_place(place_with_id.span(), place_with_id.place.clone());

        let place = restrict_capture_precision(place, &mut dummy_capture_info);

        dummy_capture_info.capture_kind = dummy_capture_kind;
        let place = restrict_repr_packed_field_ref_capture(place, &mut dummy_capture_info);
        self.fake_reads.push((place, cause, place_with_id.origins));
```

after:

```rust
        let mut dummy_capture_info =
            CaptureInfoBuf { sources: Sources::none(ctx.frame), capture_kind: dummy_capture_kind };

        let span = place_with_id.span();
        let PlaceWithOrigin { origins, place } = place_with_id;
        let place = ctx.normalize_capture_place(span, place);

        let place = restrict_capture_precision(place, &mut dummy_capture_info);

        dummy_capture_info.capture_kind = dummy_capture_kind;
        let place = restrict_repr_packed_field_ref_capture(place, &mut dummy_capture_info);
        self.fake_reads.push((place, cause, origins));
```

`consume` and `use_cloned`, before:

```rust
        let place = ctx.normalize_capture_place(place_with_id.span(), place_with_id.place.clone());

        self.capture_information.push((
            place,
            CaptureInfo { sources: place_with_id.origins, capture_kind: UpvarCapture::ByValue },
        ));
```

after (`ByUse` in `use_cloned`):

```rust
        let span = place_with_id.span();
        let PlaceWithOrigin { origins, place } = place_with_id;
        let place = ctx.normalize_capture_place(span, place);

        self.capture_information.push((
            place,
            CaptureInfoBuf { sources: origins, capture_kind: UpvarCapture::ByValue },
        ));
```

`borrow`, before:

```rust
        let capture_kind = UpvarCapture::ByRef(bk);
        let mut capture_info =
            CaptureInfo { sources: place_with_id.origins.iter().cloned().collect(), capture_kind };

        let place = ctx.normalize_capture_place(place_with_id.span(), place_with_id.place.clone());
```

after:

```rust
        let capture_kind = UpvarCapture::ByRef(bk);
        let span = place_with_id.span();
        let PlaceWithOrigin { origins, place } = place_with_id;
        let mut capture_info = CaptureInfoBuf { sources: origins, capture_kind };

        let place = ctx.normalize_capture_place(span, place);
```

`mutate` only changes its signature.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written.

### A3. `normalize_capture_place` skips what normalizing would not change (lines 939-969)

Before:

```rust
    fn normalize_capture_place(&mut self, span: Span, place: Place) -> Place {
        let place = self.infcx().deeply_resolve_ignoring_regions(place);

        // In the new solver, types in HIR `Place`s can contain unnormalized aliases,
        // which can ICE later (e.g. when projecting fields for diagnostics).
        let cause = ObligationCause::new(span);
```

After:

```rust
    fn normalize_capture_place(&mut self, span: Span, place: PlaceBuf<'f>) -> PlaceBuf<'f> {
        let place = self.infcx().deeply_resolve_ignoring_regions(place);
        // `normalize_with_universes` returns such a place as it is, with no obligation to fulfil.
        if !self.interner().renormalize_rigid_aliases() && !place.has_non_rigid_aliases() {
            return place;
        }

        // In the new solver, types in HIR `Place`s can contain unnormalized aliases,
        // which can ICE later (e.g. when projecting fields for diagnostics).
        let cause = ObligationCause::new(span);
```

The rest is unchanged (`place.clone()`, now a frame copy, is kept for the error path; `vec![]` is
the solver's universe list and allocates nothing). The early return is exactly the condition on
which `normalize_with_universes` (`next_solver/normalize.rs:36`) returns the value and no
obligations, after which the fresh `FulfillmentCtxt` has nothing to evaluate, report or drain;
so the result is the same, without the copy and the `FulfillmentCtxt` per captured use.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written; the kept `place.clone()` and `vec![]` got `// clone:`/`// alloc:` lines.

### A4. `analyze_closure` builds the result once (lines 258-528)

Lines 258-262, before:

```rust
        let mut delegate = InferBorrowKind {
            closure_def_id: closure_expr_id,
            capture_information: Default::default(),
            fake_reads: Default::default(),
        };
```

after `let mut delegate = InferBorrowKind::new(closure_expr_id, self.frame);`, and the same at
lines 552-556 (`coroutine_body_consumes_upvars`, with `coroutine_def_id`).

Lines 351-357, before:

```rust
                delegate.capture_information.push((
                    place,
                    CaptureInfo {
                        sources: smallvec![CaptureSourceStack::from_single(init.into())],
                        capture_kind: UpvarCapture::ByValue,
                    },
                ));
```

after:

```rust
                delegate.capture_information.push((
                    place,
                    CaptureInfoBuf {
                        sources: Sources::one(SourceStack::One(init.into())),
                        capture_kind: UpvarCapture::ByValue,
                    },
                ));
```

Lines 366-393, before:

```rust
        let (capture_information, closure_kind, origin) = self
            .process_collected_capture_information(capture_clause, &delegate.capture_information);

        self.compute_min_captures(closure_expr_id, capture_information);

        // We now fake capture information for all variables that are mentioned within the closure
        // We do this after handling migrations so that min_captures computes before
        if !enable_precise_capture(self.edition) {
            let mut capture_information: InferredCaptureInformation = Default::default();

            for var_hir_id in upvars.iter() {
                let place = Place {
                    base_ty: self.result.binding_ty(var_hir_id).store(),
                    base: PlaceBase::Upvar { closure: closure_expr_id, var_id: var_hir_id },
                    projections: Vec::new(),
                };

                debug!("seed place {:?}", place);

                let capture_kind = self.init_capture_kind_for_place(&place, capture_clause);
                let fake_info = CaptureInfo { sources: SmallVec::new(), capture_kind };

                capture_information.push((place, fake_info));
            }

            // This will update the min captures based on this new fake information.
            self.compute_min_captures(closure_expr_id, capture_information);
        }
```

after:

```rust
        let InferBorrowKind { closure_def_id: _, capture_information, fake_reads } = delegate;
        let (capture_information, closure_kind, origin) =
            self.process_collected_capture_information(capture_clause, capture_information);

        let mut min_captures = MinCapturesBuf::new(self.frame);
        self.compute_min_captures(closure_expr_id, capture_information, &mut min_captures);

        // We now fake capture information for all variables that are mentioned within the closure
        // We do this after handling migrations so that min_captures computes before
        if !enable_precise_capture(self.edition) {
            let mut capture_information = Vec::with_capacity_in(upvars.iter().len(), self.frame);

            for var_hir_id in upvars.iter() {
                let place = PlaceBuf::new(
                    self.result.binding_ty(var_hir_id),
                    PlaceBase::Upvar { closure: closure_expr_id, var_id: var_hir_id },
                    self.frame,
                );

                debug!("seed place {:?}", place);

                let capture_kind = self.init_capture_kind_for_place(&place, capture_clause);
                let fake_info = CaptureInfoBuf { sources: Sources::none(self.frame), capture_kind };

                capture_information.push((place, fake_info));
            }

            // This will update the min captures based on this new fake information.
            self.compute_min_captures(closure_expr_id, capture_information, &mut min_captures);
        }

        // The closure's data is inserted with its signature (`infer_closure`), and its captures
        // are computed only here: everything below reads them.
        let closure_data = self.result.closures_data.get_mut(&closure_expr_id).unwrap();
        assert!(closure_data.min_captures.is_empty(), "a closure's captures are computed once");
        closure_data.min_captures = min_captures.finish();
```

The `unwrap` is on the same entry that line 527 already unwraps unconditionally, with no return in
between, so it panics in no case the code does not today. `min_captures` starts empty as
`compute_min_captures`' map did: anon-const results are merged after `closure_analyze` (`infer.rs`
lines 273 and on).

Lines 408-411, before `self.result.closures_data.get_mut(&closure_expr_id).unwrap().kind_origin =
Some(origin);`; after:

```rust
            if let Some((node, place)) = origin {
                self.result.closures_data.get_mut(&closure_expr_id).unwrap().kind_origin =
                    Some((node, place.to_place()));
            }
```

Lines 432-463 (coroutine-closure captures by ref), before:

```rust
            let tupled_upvars_ty_for_borrow = Ty::new_tup_from_iter(
                self.interner(),
                analyze_coroutine_closure_captures(
                    self.closure_min_captures_flattened(closure_expr_id),
                    self.closure_min_captures_flattened(ExpressionStore::coroutine_for_closure(
                        closure_expr_id,
                    ))
                    // Skip the captures that are just moving the closure's args
                    // into the coroutine. These are always by move, and we append
                    // those later in the `CoroutineClosureSignature` helper functions.
                    .skip(num_args),
                    |(_, parent_capture), (_, child_capture)| {
```

after (closure body unchanged):

```rust
            let child_captures = self
                .closure_min_captures_flattened(ExpressionStore::coroutine_for_closure(closure_expr_id));
            // Skip the captures that are just moving the closure's args
            // into the coroutine. These are always by move, and we append
            // those later in the `CoroutineClosureSignature` helper functions.
            let child_captures = child_captures.get(num_args..).unwrap_or_default();
            let upvar_tys = analyze_coroutine_closure_captures(
                self.closure_min_captures_flattened(closure_expr_id),
                child_captures,
                self.frame,
                |(_, parent_capture), (_, child_capture)| {
                    ..
                },
            );
            let tupled_upvars_ty_for_borrow = Ty::new_tup(self.interner(), &upvar_tys);
```

(`get(num_args..)` is empty where `skip` was; either way the asserts in
`analyze_coroutine_closure_captures` then report the mismatch.)

Lines 513-518, before:

```rust
        let final_upvar_tys = self.final_upvar_tys(closure_expr_id);
        debug!(?closure_expr_id, ?args, ?final_upvar_tys);

        // Build a tuple (U0..Un) of the final upvar types U0..Un
        // and unify the upvar tuple type in the closure with it:
        let final_tupled_upvars_type = Ty::new_tup(self.interner(), &final_upvar_tys);
```

after: the same with `Ty::new_tup(self.interner(), final_upvar_tys)` (it is a slice now, A6).

Lines 525-528, before:

```rust
        let fake_reads = delegate.fake_reads;

        self.result.closures_data.get_mut(&closure_expr_id).unwrap().fake_reads =
            fake_reads.into_boxed_slice();
```

after (`fake_reads` was taken out of the delegate above):

```rust
        // alloc: the result's fake reads, once at their number (a slice map is `TrustedLen`)
        self.result.closures_data.get_mut(&closure_expr_id).unwrap().fake_reads = fake_reads
            .iter()
            .map(|(place, cause, sources)| (place.to_place(), *cause, sources.to_result()))
            .collect();
```

`coroutine_body_consumes_upvars` (lines 566-567), before `.process_collected_capture_information(CaptureBy::Ref,
&delegate.capture_information);`; after `.process_collected_capture_information(CaptureBy::Ref,
delegate.capture_information);`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written; the fake reads' `alloc:` comment sits above `.collect()`. The deferred-call loop is unchanged (`mem-db-infer.md` 6.6).

### A5. `process_collected_capture_information` moves its input (lines 606-666)

Before:

```rust
        capture_information: &InferredCaptureInformation,
    ) -> (InferredCaptureInformation, ClosureKind, Option<(ExprOrPatIdPacked, Place)>) {
        let mut closure_kind = ClosureKind::LATTICE_BOTTOM;
        let mut origin: Option<(ExprOrPatIdPacked, Place)> = None;

        let processed = capture_information
            .iter()
            .cloned()
            .map(|(place, mut capture_info)| {
                ..
                (place, capture_info)
            })
            .collect();

        (processed, closure_kind, origin)
```

after (the body of the map unchanged; `usage`'s `place.clone()` stays, a frame copy at most twice
per closure, when its kind goes up):

```rust
        capture_information: InferredCaptureInformation<'f>,
    ) -> (InferredCaptureInformation<'f>, ClosureKind, Option<(ExprOrPatIdPacked, PlaceBuf<'f>)>) {
        let mut closure_kind = ClosureKind::LATTICE_BOTTOM;
        let mut origin: Option<(ExprOrPatIdPacked, PlaceBuf<'f>)> = None;

        let mut processed = Vec::with_capacity_in(capture_information.len(), self.frame);
        for (place, mut capture_info) in capture_information {
            ..
            processed.push((place, capture_info));
        }

        (processed, closure_kind, origin)
```

The `usage` closure's parameter becomes `|place: &PlaceBuf<'f>|`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written (the map's body moved one level out, unchanged).

### A6. `compute_min_captures` works in the frame, in place (lines 740-937)

Before (the signature and the parts that change):

```rust
    fn compute_min_captures(
        &mut self,
        closure_def_id: ExprId,
        capture_information: InferredCaptureInformation,
    ) {
        if capture_information.is_empty() {
            return;
        }

        let mut closure_data = self.result.closures_data.remove(&closure_def_id).unwrap();
        let root_var_min_capture_list = &mut closure_data.min_captures;
        let mut dedup_sources_scratch = FxHashSet::default();

        for (mut place, capture_info) in capture_information.into_iter() {
            let var_hir_id = ..;

            let Some(min_cap_list) = root_var_min_capture_list.get_mut(&var_hir_id) else {
                let mutability = self.determine_capture_mutability(closure_def_id, &place);
                let min_cap_list = vec![CapturedPlace { place, info: capture_info, mutability }];
                root_var_min_capture_list.insert(var_hir_id, min_cap_list);
                continue;
            };
            ..
            min_cap_list.retain(|possible_descendant| {
                match determine_place_ancestry_relation(&place, &possible_descendant.place) {
                    // current place is ancestor of possible_descendant
                    PlaceAncestryRelation::Ancestor => {
                        descendant_found = true;

                        let mut possible_descendant = possible_descendant.clone();
```

after:

```rust
    fn compute_min_captures(
        &mut self,
        closure_def_id: ExprId,
        capture_information: InferredCaptureInformation<'f>,
        min_captures: &mut MinCapturesBuf<'f>,
    ) {
        if capture_information.is_empty() {
            return;
        }

        let frame = self.frame;
        let mut dedup_sources_scratch = hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);

        for (mut place, capture_info) in capture_information {
            let var_hir_id = ..;

            let Some(&root) = min_captures.index.get(&var_hir_id) else {
                let mutability = self.determine_capture_mutability(closure_def_id, &place);
                let mut min_cap_list = Vec::new_in(frame);
                min_cap_list.push(CapturedPlaceBuf { place, info: capture_info, mutability });
                let root = u32::try_from(min_captures.roots.len()).expect("fewer than 2^32 roots");
                min_captures.index.insert(var_hir_id, root);
                min_captures.roots.push((var_hir_id, min_cap_list));
                continue;
            };
            let min_cap_list = &mut min_captures.roots[root as usize].1;
            ..
            min_cap_list.retain_mut(|possible_descendant| {
                match determine_place_ancestry_relation(&place, &possible_descendant.place) {
                    // current place is ancestor of possible_descendant
                    PlaceAncestryRelation::Ancestor => {
                        descendant_found = true;
```

(`possible_descendant` is the `&mut` element, truncated in place: it is removed by this `retain`,
so the copy was only for borrowck.) Every `determine_capture_sources(.., &mut
dedup_sources_scratch)` call gains `, frame` (A7). `CapturedPlace { place, info:
updated_capture_info, mutability }` (line 872) becomes `CapturedPlaceBuf { .. }`. The sort loop,
before `for (_, captures) in &mut *root_var_min_capture_list {`; after `for (_, captures) in
min_captures.roots.iter_mut() {` (its body unchanged). The two `debug!`s print
`min_captures.roots` instead of `root_var_min_capture_list`, and the last line
`self.result.closures_data.insert(closure_def_id, closure_data);` is deleted: nothing was
removed.

The sort is `sort_by` (stable, as rustc's; equal keys must keep their order): it sorts in a stack
buffer of 4 KiB, so it allocates (globally) only for a root with more than about 39 captures.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written; all three `determine_capture_sources` calls pass `frame`.

### A7. Helpers on frame values (lines 1110-1578)

Mechanical, with `<'f>` on each function: in `restrict_repr_packed_field_ref_capture`,
`restrict_precision_for_drop_types` (`fcx: &mut InferenceContext<'db, 'f>`),
`restrict_precision_for_unsafe`, `restrict_capture_precision`, `adjust_for_move_closure`,
`adjust_for_non_move_closure`, `truncate_place_to_len_and_update_capture_kind`,
`truncate_capture_for_optimization`, `determine_capture_info` and
`determine_place_ancestry_relation`, `Place` becomes `PlaceBuf<'f>` and `CaptureInfo` becomes
`CaptureInfoBuf<'f>`; the same in `determine_capture_mutability`, `init_capture_kind_for_place`
(`place: &PlaceBuf<'f>`) and `place_for_root_variable`, whose body becomes:

```rust
    fn place_for_root_variable(&mut self, closure_def_id: ExprId, var_hir_id: BindingId) -> PlaceBuf<'f> {
        let place = PlaceBuf::new(
            self.result.binding_ty(var_hir_id),
            PlaceBase::Upvar { closure: closure_def_id, var_id: var_hir_id },
            self.frame,
        );

        // Normalize eagerly when inserting into `capture_information`, so all downstream
        // capture analysis can assume a normalized `Place`.
        self.normalize_capture_place(var_hir_id.into(), place)
    }
```

`truncate_place_to_len_and_update_capture_kind` (line 1490), before `for source in &mut
info.sources {`; after `for source in info.sources.as_mut_slice() {` (`source.truncate(len + 1)`
is `SourceStack::truncate`).

`determine_capture_sources` (lines 1440-1454), before:

```rust
fn determine_capture_sources(
    kept: &mut CaptureInfo,
    other: &mut CaptureInfo,
    dedup_sources_scratch: &mut FxHashSet<ExprOrPatIdPacked>,
) -> SmallVec<[CaptureSourceStack; 2]> {
    dedup_sources_scratch.clear();
    let mut result = mem::take(&mut kept.sources);
    result.retain(|it| dedup_sources_scratch.insert(it.final_source()));
    result.extend(
        mem::take(&mut other.sources)
            .into_iter()
            .filter(|it| dedup_sources_scratch.insert(it.final_source())),
    );
    result
}
```

after (the same sources in the same order):

```rust
fn determine_capture_sources<'f>(
    kept: &mut CaptureInfoBuf<'f>,
    other: &mut CaptureInfoBuf<'f>,
    dedup_sources_scratch: &mut hashbrown::HashSet<ExprOrPatIdPacked, FxBuildHasher, &'f Frame>,
    frame: &'f Frame,
) -> Sources<'f> {
    dedup_sources_scratch.clear();
    let mut result = mem::replace(&mut kept.sources, Sources::none(frame));
    result.retain(|it| dedup_sources_scratch.insert(it.final_source()), frame);
    for stack in mem::replace(&mut other.sources, Sources::none(frame)) {
        if dedup_sources_scratch.insert(stack.final_source()) {
            result.push(stack, frame);
        }
    }
    result
}
```

`should_reborrow_from_env_of_parent_coroutine_closure` and
`child_prefix_matches_parent_projections` read the result and stay as they are.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written, including `determine_capture_info` and `determine_capture_mutability`.

### A8. Reading a closure's captures (lines 572-589, 971-981, 1588-1638)

`final_upvar_tys`, before:

```rust
    fn final_upvar_tys(&self, closure_id: ExprId) -> Vec<Ty<'db>> {
        self.closure_min_captures_flattened(closure_id)
            .map(|captured_place| {
                ..
            })
            .collect()
    }
```

after (closure body unchanged):

```rust
    fn final_upvar_tys(&self, closure_id: ExprId) -> &'f [Ty<'db>] {
        let frame = self.frame;
        frame.alloc_slice_from_iter(self.closure_min_captures_flattened(closure_id).iter().map(
            |captured_place| {
                ..
            },
        ))
    }
```

`closure_min_captures_flattened`, before:

```rust
    fn closure_min_captures_flattened(
        &self,
        closure_expr_id: ExprId,
    ) -> impl Iterator<Item = &CapturedPlace> {
        self.result
            .closures_data
            .get(&closure_expr_id)
            .map(|closure_data| closure_data.min_captures.values().flatten())
            .into_iter()
            .flatten()
    }
```

after:

```rust
    fn closure_min_captures_flattened(&self, closure_expr_id: ExprId) -> &[CapturedPlace] {
        self.result
            .closures_data
            .get(&closure_expr_id)
            .map_or(&[], |closure_data| closure_data.min_captures.flattened())
    }
```

`analyze_coroutine_closure_captures`, before:

```rust
fn analyze_coroutine_closure_captures<'a, T>(
    parent_captures: impl IntoIterator<Item = &'a CapturedPlace>,
    child_captures: impl IntoIterator<Item = &'a CapturedPlace>,
    mut for_each: impl FnMut((usize, &'a CapturedPlace), (usize, &'a CapturedPlace)) -> T,
) -> impl Iterator<Item = T> {
    let mut result = SmallVec::<[_; 10]>::new();

    let mut child_captures = child_captures.into_iter().enumerate().peekable();
    ..
    for (parent_field_idx, parent_capture) in parent_captures.into_iter().enumerate() {
    ..
    assert_eq!(child_captures.next(), None, "leftover child captures?");

    result.into_iter()
}
```

after (each child capture gives exactly one `T`, and the last assert checks every one did):

```rust
fn analyze_coroutine_closure_captures<'a, 'f, T>(
    parent_captures: &'a [CapturedPlace],
    child_captures: &'a [CapturedPlace],
    frame: &'f Frame,
    mut for_each: impl FnMut((usize, &'a CapturedPlace), (usize, &'a CapturedPlace)) -> T,
) -> Vec<T, &'f Frame> {
    let mut result = Vec::with_capacity_in(child_captures.len(), frame);

    let mut child_captures = child_captures.iter().enumerate().peekable();
    ..
    for (parent_field_idx, parent_capture) in parent_captures.iter().enumerate() {
    ..
    assert_eq!(child_captures.next(), None, "leftover child captures?");

    result
}
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): as written.

### What `analysis.rs` still allocates

| site | what | why it stays |
|---|---|---|
| `MinCapturesBuf::finish`, A4's fake reads and `kind_origin`, `PlaceBuf::to_place`, `Sources::to_result`, `CaptureSourceStack::from_slice` | the result: per closure 2 blocks for `min_captures` and 1 for `fake_reads` (none when empty), per place 1 for its projections, per source list 1, per stack of 3 or more 1 | R4: owned, once, at exact size; the same count as `ClosureData::clone` |
| A5 `usage`'s `place.clone()` | frame copy, at most twice per closure | the kind origin outlives the capture it came from |
| A3 `place.clone()` | frame copy, only for a place with a non-rigid alias | the error path returns the unnormalized place |
| A3 `vec![]` | empty universe list | no allocation; the solver's parameter type |
| A4, A5, A6, A8 `Vec::with_capacity_in`/`Vec::new_in`, `MinCapturesBuf`'s map, A6's dedup set | frame | R2/R3 |
| A6 `sort_by` | stack scratch, global above about 39 captures of one root | stability is rustc's semantics |
| A8 `final_upvar_tys` | frame slice of the upvar types until the run's end (8 bytes each) | `Ty::new_tup` interns from a slice |
| line 200 `FxHashMap::with_hasher(FxBuildHasher)` | a `const` empty map | no allocation |
| line 532 `remove_deferred_call_resolutions` | its `Vec` | mem-db-infer-ctx (`infer.rs`): returns the removed entries; a frame `Vec` there removes it |
| `Ty::new_tup`, `Ty::new_fn_ptr`, `mk_fn_sig([], ..)` | interning | the interner's |

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): notes; `remove_deferred_call_resolutions` returns a frame `Vec` now (crates/hir-ty/src/infer.rs:2726).

## Entry points, hit paths, tracked functions

- Entry points called outside queries (R1): none. `closure_analyze` is called once, from the
  inference body (`infer.rs:273`); `infer_closure` from `infer/expr.rs:458`; `ExprUseVisitor`
  only from `analysis.rs:264, 563`. Persist's decoder (X4) is ctx's.
- Hit-path helpers (R7): none; nothing here runs on a memo hit.
- Tracked functions: none in these files.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): notes: none in these files.

## Tests

- Semantics: the closure capture tests (`crates/hir-ide/src/tests/closure_captures.rs`), MIR
  lowering and borrowck tests over closures, and persist's round-trip (X4.5) must pass unchanged;
  they cover the order of captures, sources and fake reads, nested closures, coroutine-closures
  and edition 2018. X2.6 tests `MinCaptures`.
- `CaptureSourceStack` (in `infer.rs`'s test module):

  ```rust
  #[test]
  fn capture_source_stacks_are_built_at_their_length() {
      let ids: Vec<ExprOrPatIdPacked> =
          (0u32..5).map(|it| ExprId::from_raw(la_arena::RawIdx::from(it)).into()).collect();
      for len in 1..=5 {
          let stack = CaptureSourceStack::from_slice(&ids[..len]);
          assert_eq!(&*stack, &ids[..len]);
          let mut next = ids[..len].iter().copied();
          let read = CaptureSourceStack::try_from_fn::<()>(len, || Ok(next.next().unwrap())).unwrap();
          assert_eq!(read, stack);
          if let CaptureSourceStackRepr::Many(many) = &stack.0 {
              assert_eq!(many.capacity(), len, "built once, at its length");
          }
      }
  }
  ```

- Budget: the parent's test 2 (cold inference allocates exactly what `result.clone()` does)
  covers these files when its fixture has closures. Add to its fixture:

  ```rust
  struct S { a: (String, u8), b: Vec<u8> }
  impl S {
      fn closures(&mut self, n: Option<u8>) -> u8 {
          let mut f = || { self.a.1 += 1; self.b.len() };          // captures through a deref, FnMut
          let g = move || match n { Some(x) if x > 1 => x, _ => 0 }; // a fake read, a guard
          let h = || { let k = || self.a.0.len(); k() };            // a nested closure
          let a = async || self.a.1;                                // a coroutine-closure
          drop(a);
          f() as u8 + g() + h() as u8
      }
  }
  ```

  and the same body in an edition-2018 crate (the fake-capture path of A4).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-closure (21:50): no test lives in my files: the `CaptureSourceStack` test is for `infer.rs`'s test module (told db-mem-infer-a-ctx) and the budget fixture for the parent's `tests/budget.rs` (told db-mem-infer-a). Added beside the code: `expr_use_visitor.rs` `mod tests` (`SourceStack` push/truncate/`of`/`to_result` at lengths 1 to 6, `Sources` push/retain/`of`/`to_result` order). No new `crates/hir-ide` test: nothing to file.
> applied by db-mem-infer-a (21:45): budget fixture in `crates/hir-ty/src/tests/budget.rs`, in crate `main` and again in an `edition:2018` crate `old`. Differences: minicore has no `String`/`Vec`, so `a.0` and `b` are two local non-`Copy` types with a `len(&self)`; `drop(a)` became `let _a = a;` (minicore's `drop` flag isn't on); `async_fn, future` added to the minicore line.

## Notes for other parts

- mem-db-infer-ctx: `obligations_for_self_ty`'s `SmallVec<[_; 4]>` spills on real closures (see
  the `closure.rs` table); `remove_deferred_call_resolutions` returns a global `Vec`;
  `InferenceContext`'s `Debug` must print with the frame field.
- mem-db-solver: the `Elaborator`'s stack and visited set, `PredicateObligations`,
  `Tys::new_from_iter`'s `SmallVec<[_; 8]>`, and the `FulfillmentCtxt` that
  `deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals` makes per call (A3
  leaves it only for places with aliases).
- mem-db-lower: `lower_closure_params` returns `Vec<Option<Ty>>`.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): each settled: `obligations_for_self_ty` stays (ctx U3), `remove_deferred_call_resolutions` framed (crates/hir-ty/src/infer.rs:2726), `lower_closure_params` takes an `Extend` (crates/hir-ty/src/lower.rs:1195); the solver's are mem-db-solver's.

## Speed

Sizes on 64-bit: `Projection` 16 bytes; `PlaceBuf` 48 (base type 4, base 12, frame `Vec` 32);
`SourceStack` and `Sources` at most 40 each; `PlaceWithOrigin` about 88; a captured use in
`capture_information` about 96; `CapturedPlaceBuf` about 104. Result: `Place` 32 (was 40),
`CapturedPlace` about 56 (was about 96).

Per 1,000-expression body, taking 10 closures holding 300 of the expressions, 150 place chains
with projections, 100 copies of such places, 30 source stacks of 3 or more, 60 captured uses, 20
min captures and 10 fake reads (the closure-heavy end of real code):

| temporary | bytes per run | frame allocations | before (global allocations) |
|---|---|---|---|
| projections of place chains (`push_projection`) | 150 × 64 = 9.6 KB | 150 | 150, plus regrowth past 4 |
| copies of places with projections | 100 × about 40 = 4 KB | 100 | 100, plus 1 per spilled origin list |
| source stacks of 3 or more | 30 × 16 = 0.5 KB | 30 | 30, plus `ThinVec` regrowth |
| `capture_information`, the processed copy | 2 × 60 × 96 = 11.5 KB (with doubling, up to 23 KB) | about 14 | about 14, plus a clone of every entry with its projections and sources (about 120) |
| `MinCapturesBuf` (roots, lists, index), dedup set | about 7 KB | about 30 | `IndexMap` per closure (2 + growth), a `Vec` per root, a set per call |
| fake reads, upvar types | about 1.2 KB | about 20 | about 20 |
| A3's skipped copies and `FulfillmentCtxt`s | 0 | 0 | 60 copies and 60 `FulfillmentCtxt`s |
| total | about 35 to 45 KB, reclaimed at the run's end | about 345 | about 600, each with a free |

At 2 ns a frame allocation against 15 to 25 ns global (plus the free), that is about 0.7 µs per
run against about 12 µs before, not counting the 60 `FulfillmentCtxt`s A3 no longer makes. The
result itself is about 75 global allocations (20 captures × projections and sources, their
stacks of 3 or more, 3 per closure), each made once at its size, where before there were about
twice as many with growth and `shrink_to_fit` reallocations.

The bound: categorising an expression costs at least one `deeply_resolve_ignoring_regions` and
usually a `structurally_resolve_type`, 50 to 100 ns; the temporaries now add about 2.3 ns per
closure-body expression (0.7 µs / 300), 2 to 5% of the pass, so the pass is within 10% of doing
its type work alone. The remaining allocations are the place values the algorithm needs to exist
at once (a place and its copy for each scrutinee, arm, subpattern and adjustment); keeping two
projections inline would drop the 150 chain allocations (0.3 µs, under 1% of the pass) and add a
branch to every projection access, so it is not taken. The result's allocations are the ones the
parent's budget allows (its clone count).

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:38): notes; nothing to edit.
