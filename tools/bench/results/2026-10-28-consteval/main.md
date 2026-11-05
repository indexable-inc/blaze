# consteval_ui.py

Evaluation: **727 of 767** units the same as rustc (94.8%), of 1729 units in 1595 tests of tests/ui/{consts,const-generics}; 962 out of scope.

Values (run-pass, uitest.py): **222 of 222** match (100.0%); 83 left out.

## Differences of evaluation, largest class first

An item is one constant or static; a unit, one revision of a test.

| items | units | kind | what | e.g. |
|---|---|---|---|---|
| 9 | 1 | rustc fails where blaze evaluates | in-bounds pointer arithmetic failed: attempting to | consts/offset_ub.rs |
| 7 | 1 | rustc fails where blaze evaluates | `_` called on two different pointers | consts/offset_from_ub.rs |
| 7 | 6 | blaze fails where rustc evaluates | MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol: | const-generics/gca/basic-different-definitions.rs, const-generics/gca/basic.rs, const-generics/gca/coherence-ok.rs |
| 5 | 1 | rustc fails where blaze evaluates | `_` called when first pointer is | consts/offset_from_ub.rs |
| 3 | 2 | rustc fails where blaze evaluates | overflowing pointer arithmetic: the total offset | consts/offset_ub.rs, consts/slice-index-overflow-issue-130284.rs |
| 3 | 2 | rustc fails where blaze evaluates | unable to read parts of a | consts/const-eval/ptr_fragments_mixed.rs, consts/const-eval/read_partial_ptr.rs |
| 3 | 1 | rustc fails where blaze evaluates | accessing memory based on pointer with | consts/const-eval/raw-pointer-ub.rs |
| 3 | 1 | rustc fails where blaze evaluates | reading memory at ALLOC$ID[0x0..0x8], but memory | consts/const-eval/union-ice.rs |
| 3 | 1 | rustc fails where blaze evaluates | incorrect layout on deallocation: ALLOC$ID has | consts/const-eval/heap/dealloc_intrinsic_incorrect_layout.rs |
| 2 | 2 | rustc fails where blaze evaluates | unable to turn pointer into integer | consts/const_transmute_type_id.rs, consts/issue-miri-1910.rs |
| 2 | 1 | rustc fails where blaze evaluates | overflow computing total size of `_` | consts/copy-intrinsic.rs |
| 2 | 2 | rustc fails where blaze evaluates | memory access failed: attempting to access | consts/missing_span_in_backtrace.rs, consts/const-eval/raw-pointer-ub.rs |
| 2 | 1 | blaze fails where rustc evaluates | MirEvalError(InternalError("atomic intrinsic arg2 is not provided") | consts/const-eval/atomic.rs |
| 2 | 2 | rustc fails where blaze evaluates | memory access failed: ALLOC$ID has been | consts/const-eval/issue-49296.rs, consts/const-eval/heap/dealloc_intrinsic_duplicate.rs |
| 2 | 1 | rustc fails where blaze evaluates | writing through a pointer that was | consts/const-eval/ub-write-through-immutable.rs |
| 2 | 2 | blaze fails where rustc evaluates | MirLowerError(NotSupported("monomorphization resulted in errors: [(); {const error}]")) | const-generics/gca/basic-different-definitions.rs, const-generics/gca/basic.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(NotSupported("unknown external function floorf128") | consts/const-float-intrinsics.rs |
| 1 | 1 | rustc fails where blaze evaluates | evaluation panicked: null-ness of this pointer | consts/const-ptr-is-null.rs |
| 1 | 1 | rustc fails where blaze evaluates | pointer not dereferenceable: pointer must point | consts/const_transmute_type_id2.rs |
| 1 | 1 | rustc fails where blaze evaluates | invalid `_` value: the hash does | consts/const_transmute_type_id5.rs |
| 1 | 1 | rustc fails where blaze evaluates | invalid `_` value: not all bytes | consts/const_transmute_type_id6.rs |
| 1 | 1 | rustc fails where blaze evaluates | constructing invalid value of type ChildStdin: | consts/issue-64506.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(UndefinedBehavior("field of an uninhabited type") | consts/let-irrefutable-pattern-ice-120337.rs |
| 1 | 1 | rustc fails where blaze evaluates | exact_div: 1_isize cannot be divided by | consts/offset_from_ub.rs |
| 1 | 1 | rustc fails where blaze evaluates | `_` called when first pointer has | consts/offset_from_ub.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(UndefinedBehavior("`unreachable` intrinsic executed") | consts/ptr_comparisons.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirLowerError(UnresolvedMethod("bar")) | consts/trait_alias_method_call.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(InternalError("atomic intrinsic arg1 is not provided") | consts/const-eval/atomic.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(InternalError("atomic intrinsic generic arg is not provided") | consts/const-eval/atomic.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(UndefinedBehavior("out of bound memory write") | consts/const-eval/issue-91827-extern-types-field-offset.rs |
| 1 | 1 | rustc fails where blaze evaluates | accessing memory with alignment N, but | consts/const-eval/raw-pointer-ub.rs |
| 1 | 1 | rustc fails where blaze evaluates | reading memory at ALLOC$ID[0x1..0x2], but memory | consts/const-eval/ub-enum-overwrite.rs |
| 1 | 1 | rustc fails where blaze evaluates | interpreting an invalid N-bit value as | consts/const-eval/ub-invalid-values.rs |
| 1 | 1 | rustc fails where blaze evaluates | constructing invalid value of type empty::Empty: | consts/const-eval/validate_uninhabited_zsts.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(NotSupported("intrinsic volatile_store is not implemented") | consts/const-eval/volatile.rs |
| 1 | 1 | rustc fails where blaze evaluates | invalid align passed to `_`: N | consts/const-eval/heap/dealloc_intrinsic_incorrect_layout.rs |
| 1 | 1 | rustc fails where blaze evaluates | pointer passed to `_` does not | consts/const-eval/heap/make-global-other.rs |
| 1 | 1 | rustc fails where blaze evaluates | attempting to call `_` twice on | consts/const-eval/heap/make-global-twice.rs |
| 1 | 1 | rustc fails where blaze evaluates | writing to ALLOC$ID which is read-only | consts/const-eval/heap/ptr_made_global_mutated.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(ConstEvalError("ADD1", MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { ki | const-generics/gca/basic.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirLowerError(NotSupported("monomorphization resulted in errors: PC<{type error}, {const error}>")) | const-generics/mgca/adt_expr_infers_from_value.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirLowerError(NotSupported("monomorphization resulted in errors: StructWithConstParam<{const error}>")) | const-generics/mgca/type_const-generic-param-in-type.rs#gate |
| 1 | 1 | blaze fails where rustc evaluates | MirLowerError(NotSupported("monomorphization resulted in errors: [{type error}; ValueConst { ty: usize, value: ValTreeIn | const-generics/mgca/type_const-generic-param-in-type.rs#gate |

## Differences of values (run-pass)


## Out of scope, by reason

- 339 rustc reports an error besides const evaluation
- 72 rustc reports E0308 besides const evaluation
- 55 an error of a generic or anonymous constant's instance
- 46 rustc reports E0277 besides const evaluation
- 43 an auxiliary crate
- 25 rustc reports E0425 besides const evaluation
- 25 rustc reports E0741 besides const evaluation
- 22 rustc reports E0658 besides const evaluation
- 21 rustc reports E0770 besides const evaluation
- 19 rustc reports E0493 besides const evaluation
- 19 rustc reports E0716 besides const evaluation
- 17 the flag `-Zunleash-the-miri-inside-of-you`
- 15 rustc reports E0015 besides const evaluation
- 14 no expected output, and not a passing test
- 14 rustc reports E0284 besides const evaluation
- 13 an E0080 that names no constant
- 13 rustc reports E0107 besides const evaluation
- 11 rustc reports E0435 besides const evaluation
- 11 rustc reports E0747 besides const evaluation
- 10 rustc reports E0391 besides const evaluation
- 9 rustc reports E0038 besides const evaluation
- 8 the flag `-Z tiny-const-eval-limit`
- 6 rustc's error names an item blaze does not evaluate (in a body, say)
- 5 rustc reports E0005 besides const evaluation
- 5 the flag `-Zextra-const-ub-checks`
- 5 rustc reports E0492 besides const evaluation
- 5 rustc reports E0119 besides const evaluation
- 5 rustc reports E0204 besides const evaluation
- 4 rustc reports E0764 besides const evaluation
- 4 rustc reports E0573 besides const evaluation
- 4 rustc reports E0271 besides const evaluation
- 4 rustc reports E0275 besides const evaluation
- 3 rustc reports E0379 besides const evaluation
- 3 rustc reports E0403 besides const evaluation
- 3 rustc reports E0282 besides const evaluation
- 3 rustc reports E0158 besides const evaluation
- 3 the flag `-Zno-profiler-runtime`
- 3 rustc reports E0637 besides const evaluation
- 3 rustc reports E0053 besides const evaluation
- 2 rustc reports E0606 besides const evaluation
- 2 rustc reports E0609 besides const evaluation
- 2 rustc reports E0381 besides const evaluation
- 2 rustc reports E0597 besides const evaluation
- 2 rustc reports E0106 besides const evaluation
- 2 the flag `-Zthreads`
- 2 rustc reports E0261 besides const evaluation
- 2 rustc reports E0599 besides const evaluation
- 2 rustc reports E0392 besides const evaluation
- 2 rustc reports E0191 besides const evaluation
- 2 rustc reports E0121 besides const evaluation
- 2 rustc reports E0423 besides const evaluation
- 2 rustc reports E0046 besides const evaluation
- 1 rustc reports E0618 besides const evaluation
- 1 `only-nightly`
- 1 rustc reports E0532 besides const evaluation
- 1 rustc reports E0572 besides const evaluation
- 1 rustc reports E0367 besides const evaluation
- 1 `needs-asm-support`
- 1 rustc reports E0310 besides const evaluation
- 1 rustc reports E0515 besides const evaluation
- 1 rustc reports E0326 besides const evaluation
- 1 rustc reports E0745 besides const evaluation
- 1 rustc reports E0607 besides const evaluation
- 1 the flag `-Ztiny-const-eval-limit`
- 1 rustc reports E0512 besides const evaluation
- 1 the flag `-Zunpretty`
- 1 `only-wasm32`
- 1 rustc reports E0133 besides const evaluation
- 1 rustc reports E0369 besides const evaluation
- 1 rustc reports E0428 besides const evaluation
- 1 rustc reports E0004 besides const evaluation
- 1 the flag `-Zforce-unstable-if-unmarked`
- 1 the flag `-Z unleash-the-miri-inside-of-you`
- 1 rustc reports E0575 besides const evaluation
- 1 rustc reports E0044 besides const evaluation
- 1 the flag `-Zinline-mir`
- 1 rustc reports E0557 besides const evaluation
- 1 rustc reports E0118 besides const evaluation
- 1 rustc reports E0222 besides const evaluation
- 1 rustc reports E0393 besides const evaluation
- 1 rustc reports E0229 besides const evaluation
- 1 rustc reports E0220 besides const evaluation
- 1 rustc reports E0735 besides const evaluation
- 1 rustc reports E0128 besides const evaluation
- 1 the flag `-Z unpretty`
- 1 rustc reports E0401 besides const evaluation
- 1 rustc reports E0049 besides const evaluation
- 1 rustc reports E0446 besides const evaluation
- 1 rustc reports E0407 besides const evaluation
- 1 rustc reports E0432 besides const evaluation
- 1 rustc reports E0433 besides const evaluation
- 1 rustc reports E0283 besides const evaluation
- 1 rustc reports E0422 besides const evaluation
- 1 rustc reports E0559 besides const evaluation
- 1 rustc reports E0560 besides const evaluation
- 1 rustc reports E0223 besides const evaluation
- 1 rustc reports E0109 besides const evaluation
- 1 rustc reports E0449 besides const evaluation
- 1 the flag `-Zunstable-options`

## Every unit that differs

- consts/const-float-intrinsics.rs: blaze fails where rustc evaluates: _: MirEvalError(NotSupported("unknown external function floorf128")
- consts/const-ptr-is-null.rs: rustc fails where blaze evaluates: MAYBE_NULL: evaluation panicked: null-ness of this pointer
- consts/const_transmute_type_id.rs: rustc fails where blaze evaluates: _: unable to turn pointer into integer
- consts/const_transmute_type_id2.rs: rustc fails where blaze evaluates: _: pointer not dereferenceable: pointer must point
- consts/const_transmute_type_id5.rs: rustc fails where blaze evaluates: _: invalid `_` value: the hash does
- consts/const_transmute_type_id6.rs: rustc fails where blaze evaluates: X: invalid `_` value: not all bytes
- consts/copy-intrinsic.rs: rustc fails where blaze evaluates: COPY_NONOVERLAPPING_SIZE_OVERFLOW: overflow computing total size of `_`; rustc fails where blaze evaluates: COPY_SIZE_OVERFLOW: overflow computing total size of `_`
- consts/issue-64506.rs: rustc fails where blaze evaluates: FOO: constructing invalid value of type ChildStdin:
- consts/issue-miri-1910.rs: rustc fails where blaze evaluates: C: unable to turn pointer into integer
- consts/let-irrefutable-pattern-ice-120337.rs: blaze fails where rustc evaluates: C: MirEvalError(UndefinedBehavior("field of an uninhabited type")
- consts/missing_span_in_backtrace.rs: rustc fails where blaze evaluates: X: memory access failed: attempting to access
- consts/offset_from_ub.rs: rustc fails where blaze evaluates: DIFFERENT_ALLOC: `_` called on two different pointers; rustc fails where blaze evaluates: DIFFERENT_ALLOC_UNSIGNED: `_` called on two different pointers; rustc fails where blaze evaluates: DIFFERENT_INT: `_` called on two different pointers; rustc fails where blaze evaluates: NOT_MULTIPLE_OF_SIZE: exact_div: 1_isize cannot be divided by; rustc fails where blaze e
- consts/offset_ub.rs: rustc fails where blaze evaluates: AFTER_ARRAY: in-bounds pointer arithmetic failed: attempting to; rustc fails where blaze evaluates: AFTER_END: in-bounds pointer arithmetic failed: attempting to; rustc fails where blaze evaluates: BEFORE_START: in-bounds pointer arithmetic failed: attempting to; rustc fails where blaze evaluates: DANGLING: in-bounds pointer arithmetic failed: attempting to; rust
- consts/ptr_comparisons.rs: blaze fails where rustc evaluates: _: MirEvalError(UndefinedBehavior("`unreachable` intrinsic executed")
- consts/slice-index-overflow-issue-130284.rs: rustc fails where blaze evaluates: C: overflowing pointer arithmetic: the total offset
- consts/trait_alias_method_call.rs: blaze fails where rustc evaluates: _: MirLowerError(UnresolvedMethod("bar"))
- consts/const-eval/atomic.rs: blaze fails where rustc evaluates: ATOMIC_INT: MirEvalError(InternalError("atomic intrinsic arg2 is not provided"); blaze fails where rustc evaluates: ATOMIC_PTR: MirEvalError(InternalError("atomic intrinsic arg1 is not provided"); blaze fails where rustc evaluates: BOOL: MirEvalError(InternalError("atomic intrinsic arg2 is not provided"); blaze fails where rustc evaluates: FENCES: MirEvalError(In
- consts/const-eval/issue-49296.rs: rustc fails where blaze evaluates: X: memory access failed: ALLOC$ID has been
- consts/const-eval/issue-91827-extern-types-field-offset.rs: blaze fails where rustc evaluates: NEWTYPE: MirEvalError(UndefinedBehavior("out of bound memory write")
- consts/const-eval/ptr_fragments_mixed.rs: rustc fails where blaze evaluates: APTR: unable to read parts of a; rustc fails where blaze evaluates: BPTR: unable to read parts of a
- consts/const-eval/raw-pointer-ub.rs: rustc fails where blaze evaluates: MISALIGNED_COPY: accessing memory with alignment N, but; rustc fails where blaze evaluates: MISALIGNED_FIELD: accessing memory based on pointer with; rustc fails where blaze evaluates: MISALIGNED_LOAD: accessing memory based on pointer with; rustc fails where blaze evaluates: MISALIGNED_STORE: accessing memory based on pointer with; rustc fails where blaze evalua
- consts/const-eval/read_partial_ptr.rs: rustc fails where blaze evaluates: PTR_BYTES_MIX: unable to read parts of a
- consts/const-eval/ub-enum-overwrite.rs: rustc fails where blaze evaluates: _: reading memory at ALLOC$ID[0x1..0x2], but memory
- consts/const-eval/ub-invalid-values.rs: rustc fails where blaze evaluates: _: interpreting an invalid N-bit value as
- consts/const-eval/ub-write-through-immutable.rs: rustc fails where blaze evaluates: WRITE_AFTER_CAST: writing through a pointer that was; rustc fails where blaze evaluates: WRITE_AFTER_TRANSMUTE: writing through a pointer that was
- consts/const-eval/union-ice.rs: rustc fails where blaze evaluates: FIELD3: reading memory at ALLOC$ID[0x0..0x8], but memory; rustc fails where blaze evaluates: FIELD_PATH: reading memory at ALLOC$ID[0x0..0x8], but memory; rustc fails where blaze evaluates: FIELD_PATH2: reading memory at ALLOC$ID[0x0..0x8], but memory
- consts/const-eval/validate_uninhabited_zsts.rs: rustc fails where blaze evaluates: BAR: constructing invalid value of type empty::Empty:
- consts/const-eval/volatile.rs: blaze fails where rustc evaluates: _WRITE: MirEvalError(NotSupported("intrinsic volatile_store is not implemented")
- consts/const-eval/heap/dealloc_intrinsic_duplicate.rs: rustc fails where blaze evaluates: _X: memory access failed: ALLOC$ID has been
- consts/const-eval/heap/dealloc_intrinsic_incorrect_layout.rs: rustc fails where blaze evaluates: _W: invalid align passed to `_`: N; rustc fails where blaze evaluates: _X: incorrect layout on deallocation: ALLOC$ID has; rustc fails where blaze evaluates: _Y: incorrect layout on deallocation: ALLOC$ID has; rustc fails where blaze evaluates: _Z: incorrect layout on deallocation: ALLOC$ID has
- consts/const-eval/heap/make-global-other.rs: rustc fails where blaze evaluates: Y: pointer passed to `_` does not
- consts/const-eval/heap/make-global-twice.rs: rustc fails where blaze evaluates: Y: attempting to call `_` twice on
- consts/const-eval/heap/ptr_made_global_mutated.rs: rustc fails where blaze evaluates: A: writing to ALLOC$ID which is read-only
- const-generics/gca/basic-different-definitions.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:; blaze fails where rustc evaluates: ARR: MirLowerError(NotSupported("monomorphization resulted in errors: [(); {const error}]")); blaze fails where rustc evaluates: INC: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BareP
- const-generics/gca/basic.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:; blaze fails where rustc evaluates: ARR: MirLowerError(NotSupported("monomorphization resulted in errors: [(); {const error}]")); blaze fails where rustc evaluates: INC: MirEvalError(ConstEvalError("ADD1", MirLowerError(UnresolvedName { o
- const-generics/gca/coherence-ok.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/gca/generic-free-const.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/mgca/adt_expr_infers_from_value.rs: blaze fails where rustc evaluates: PC: MirLowerError(NotSupported("monomorphization resulted in errors: PC<{type error}, {const error}>")); blaze fails where rustc evaluates: WRAP: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/mgca/paren.rs: blaze fails where rustc evaluates: A: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/mgca/type_const-generic-param-in-type.rs#gate: blaze fails where rustc evaluates: BAR: MirLowerError(NotSupported("monomorphization resulted in errors: StructWithConstParam<{const error}>")); blaze fails where rustc evaluates: FOO: MirLowerError(NotSupported("monomorphization resulted in errors: [{type error}; ValueConst { ty: usize, value: ValTreeIn
