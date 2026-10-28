# consteval_ui.py

Evaluation: **710 of 767** units the same as rustc (92.6%), of 1729 units in 1595 tests of tests/ui/{consts,const-generics}; 962 out of scope.

Values (run-pass, uitest.py): **0 of 223** match (0.0%); 82 left out.

## Differences of evaluation, largest class first

An item is one constant or static; a unit, one revision of a test.

| items | units | kind | what | e.g. |
|---|---|---|---|---|
| 72 | 1 | rustc fails where blaze evaluates | attempt to compute `_`, which would | consts/overflowing-consts.rs#opt |
| 40 | 1 | rustc fails where blaze evaluates | overflowing shift by N in `_` | consts/const-int-unchecked.rs |
| 24 | 1 | rustc fails where blaze evaluates | attempt to shift left by `_`, | consts/overflowing-consts.rs#opt |
| 24 | 1 | rustc fails where blaze evaluates | attempt to shift right by `_`, | consts/overflowing-consts.rs#opt |
| 14 | 11 | blaze fails where rustc evaluates | MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented")) | const-generics/gca/basic-different-definitions.rs, const-generics/gca/basic.rs, const-generics/mgca/adt_expr_infers_from_value.rs |
| 9 | 1 | rustc fails where blaze evaluates | in-bounds pointer arithmetic failed: attempting to | consts/offset_ub.rs |
| 7 | 1 | rustc fails where blaze evaluates | `_` called on two different pointers | consts/offset_from_ub.rs |
| 6 | 1 | blaze fails where rustc evaluates | MirEvalError(NotSupported("intrinsic variant_count is not implemented") | consts/const-variant-count.rs |
| 5 | 1 | rustc fails where blaze evaluates | `_` called when first pointer is | consts/offset_from_ub.rs |
| 5 | 5 | rustc fails where blaze evaluates | modifying a static's initial value from | consts/static_mut_containing_mut_ref2.rs, consts/static_mut_containing_mut_ref3.rs, consts/write-to-static-mut-in-static.rs |
| 5 | 4 | blaze fails where rustc evaluates | MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol: | const-generics/gca/basic-different-definitions.rs, const-generics/gca/basic.rs, const-generics/gca/coherence-ok.rs |
| 3 | 2 | rustc fails where blaze evaluates | overflowing pointer arithmetic: the total offset | consts/offset_ub.rs, consts/slice-index-overflow-issue-130284.rs |
| 3 | 2 | rustc fails where blaze evaluates | encountered static that tried to access | consts/recursive-static-write.rs, consts/write-to-static-mut-in-static.rs |
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
| 1 | 1 | rustc fails where blaze evaluates | evaluation panicked: unsafe precondition(s) violated: slice::get_unchecked | consts/const-eval/ub-slice-get-unchecked.rs |
| 1 | 1 | rustc fails where blaze evaluates | constructing invalid value of type empty::Empty: | consts/const-eval/validate_uninhabited_zsts.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(NotSupported("intrinsic volatile_store is not implemented") | consts/const-eval/volatile.rs |
| 1 | 1 | rustc fails where blaze evaluates | invalid align passed to `_`: N | consts/const-eval/heap/dealloc_intrinsic_incorrect_layout.rs |
| 1 | 1 | rustc fails where blaze evaluates | pointer passed to `_` does not | consts/const-eval/heap/make-global-other.rs |
| 1 | 1 | rustc fails where blaze evaluates | attempting to call `_` twice on | consts/const-eval/heap/make-global-twice.rs |
| 1 | 1 | rustc fails where blaze evaluates | writing to ALLOC$ID which is read-only | consts/const-eval/heap/ptr_made_global_mutated.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirEvalError(NotSupported("unknown simd intrinsic masked_store") | consts/const-eval/simd/simd-packed-non-pow2.rs |
| 1 | 1 | blaze fails where rustc evaluates | MirLowerError(NotSupported("monomorphization resulted in errors: PC<{type error}, {const error}>")) | const-generics/mgca/adt_expr_infers_from_value.rs |

## Differences of values (run-pass)

- crash consts/arithmetic-expr-in-array-len.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/assoc-const.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/bool-comparison-in-const-array-len.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/cast-discriminant-zst-enum.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/chained-constants-stackoverflow.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/check_const-feature-gated.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-adt-align-mismatch.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-autoderef.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-big-enum.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-binops.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-bitshift-rhs-inference.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-block-cross-crate-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-block-non-item-statement-3.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-block-non-item-statement-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-block.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-blocks/const-repeat.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-blocks/fn-call-in-const.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-blocks/run-pass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-byte-str-cast.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-cast-ptr-int.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-cast.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-closure-fn-trait-object.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-const.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-contents.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-deref.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-byref-self.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-byref.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-cast.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-ptr.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-struct.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-struct2.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-structlike.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-tuplestruct.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-vec-index.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-vec-ptr.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-enum-vector.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-err-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/enum_discr.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/issue-64908.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/issue-64970.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/nrvo.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/ptr_fragments.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/strlen.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/variance-const-eval-cycle.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-eval/write-to-uninhabited-enum-variant.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-expr-in-fixed-length-vec.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-expr-in-vec-repeat.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-extern-fn/const-extern-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-extern-function.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fields-and-indexing.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn-const-eval.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn-method.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn-nested.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn-stability-calls.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn-val.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-index-feature-gate.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-arithmetic.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-conversion-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-overflowing-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-rotate-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-saturating-arith.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-sign-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-wrapping-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-item-with-block-body/macro-codegen.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-item-with-block-body/static.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-labeled-break.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-meth-pattern.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-mut-refs-crate.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-needs_drop.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-negation.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-negative.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-nullary-enum.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-nullary-univariant-enum.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-pattern-variant.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-rec-and-tup.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-region-ptrs-noncopy.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-region-ptrs.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-repeated-values.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-size_of-align_of.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-size_of_val-align_of_val.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-struct-offsets.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-struct.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-trait-to-trait.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-tuple-struct.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-typeid-of-rpass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-unit-struct.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-unsafe-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-vec-of-fns.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-vec-syntax.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-vecs-and-slices.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_constructor/const-construct-call.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_constructor/const_constructor_qpath.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_discriminant.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_fn_unsize.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_in_pattern/cross-crate-pass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_in_pattern/custom-eq-branch-pass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_in_pattern/cyclic-const-dag-pass.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_in_pattern/issue-62614.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_in_pattern/null-raw-ptr-issue-119270.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_let_eq.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_let_eq_float.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_let_promote.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_refs_to_static.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const_unsafe_unreachable.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/consts-in-patterns.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/control-flow/basics.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/control-flow/short-circuit-let.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/control-flow/short-circuit.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/deref_in_pattern.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/ice-48279.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-105536-const-val-roundtrip-ptr-eq.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-13902.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-17074.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-17718-borrow-interior.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-17718.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-17756.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-19244.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-21721.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-23833.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-23968-const-not-overflow.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-27890.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-29914-2.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-29914.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-29927-1.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-29927.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-33537.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-37222.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-37550.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-37991.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-44255.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-46553.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-54582.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-58435-ice-with-assoc-const.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-69532.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-70773-mir-typeck-lt-norm.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-90762.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-broken-mir.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/load-preserves-partial-init.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/locals-in-const-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/match-const-fn-structs.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/method-reference-in-const-dyn-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/module-const-array-size-13763.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/mozjs-error.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/non-scalar-cast.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/offset.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/offset_from.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/oncecell-const-init-57781.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/packed_pattern.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/packed_pattern2.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/promote_borrowed_field.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/promoted_const_call4.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/promotion-mutable-ref.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/references.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/repeat_match.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/return-in-const-fn.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/rvalue-static-promotion.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/short-circuit-or-with-const-bool.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/signed_enum_discr.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/static-lifetime-inference-from-const-array.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/static-mut-refs.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/static-raw-pointer-interning.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/static-raw-pointer-interning2.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/static-ref-to-const-array-element.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/std/iter.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/transmute-const.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/tuple-struct-constructors.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/write_to_mut_ref_dest.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/zst_no_llvm_alloc.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/const-int-arithmetic-overflow.rs: blaze exited: o's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteva
- crash consts/const_in_pattern/issue-73431.rs: blaze exited: argo's configuration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/const
- crash consts/issue-64059.rs#noopt: blaze exited: rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-consts/native --cfg
- crash consts/issue-64059.rs#opt: blaze exited: uration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-const
- crash consts/issue-64059.rs#opt_with_overflow_checks: blaze exited: as no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-consts/native --cfg opt_with_overflow_checks -C 
- crash const-generics/array-wrapper-struct-ctor.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/broken-mir-1.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/broken-mir-2.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/coerce_unsized_array.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/concrete-const-as-fn-arg.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/concrete-const-impl-method.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/const-arg-in-fn.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/const-fn-with-const-param.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/const-generic-type_name.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/cross_crate_complex.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/complex-unord-param.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/const-default.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/const-param-as-default-value.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/const-param-in-ty-defaults.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/repr-c-issue-82792.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/rp_impl_trait.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/simple-defaults.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/trait_object_lt_defaults.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/defaults/trait_objects.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/dyn-supertraits.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/early/const-param-hygiene.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/generic_arg_infer/array-repeat-expr.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/generic_arg_infer/dont-use-defaults.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/impl-const-generic-struct.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/infer_arg_from_pat.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/issues/issue-61432.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/issues/issue-62187-encountered-polymorphic-const.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/issues/issue-69654-run-pass.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/issues/issue-70125-1.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/issues/issue-70125-2.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/min_const_generics/const_fn_in_generics.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/min_const_generics/inferred_const.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/min_const_generics/macro.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/min_const_generics/type_and_const_defaults.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/promotion.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-after-const-ok.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/issue-61936.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/issue-63695.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/issue-69816.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/issue-70507.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/issue-71805.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/non-local.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/qpath.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type-dependent/simple.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/type_of_anon_const.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/uninferred-consts-during-codegen-1.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/uninferred-consts-during-codegen-2.rs: blaze exited: onfiguration gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-
- crash const-generics/const-argument-cross-crate.rs#full: blaze exited: n gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-const-gener
- crash const-generics/const-argument-cross-crate.rs#min: blaze exited: on gives rustc

Caused by:
    blaze has no answer to `rustc --print link-args -C save-temps -C link-dead-code --cap-lints allow -L native=/home/runner/work/_temp/consteval/ui-const-gene

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
- consts/const-int-unchecked.rs: rustc fails where blaze evaluates: SHL_I128: overflowing shift by N in `_`; rustc fails where blaze evaluates: SHL_I128_NEG: overflowing shift by N in `_`; rustc fails where blaze evaluates: SHL_I128_NEG_RANDOM: overflowing shift by N in `_`; rustc fails where blaze evaluates: SHL_I16: overflowing shift by N in `_`; rustc fails where blaze evaluates: SHL_I16_NEG: overflowing shift by N in `_`; rus
- consts/const-ptr-is-null.rs: rustc fails where blaze evaluates: MAYBE_NULL: evaluation panicked: null-ness of this pointer
- consts/const-variant-count.rs: blaze fails where rustc evaluates: NO_ICE_BOOL: MirEvalError(NotSupported("intrinsic variant_count is not implemented"); blaze fails where rustc evaluates: NO_ICE_PRIM: MirEvalError(NotSupported("intrinsic variant_count is not implemented"); blaze fails where rustc evaluates: NO_ICE_STRUCT: MirEvalError(NotSupported("intrinsic variant_count is not implemented"); blaze fails where rustc evaluates: 
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
- consts/overflowing-consts.rs#opt: rustc fails where blaze evaluates: _NI128_ADD: attempt to compute `_`, which would; rustc fails where blaze evaluates: _NI128_ADD_P: attempt to compute `_`, which would; rustc fails where blaze evaluates: _NI128_MUL: attempt to compute `_`, which would; rustc fails where blaze evaluates: _NI128_MUL_P: attempt to compute `_`, which would; rustc fails where blaze evaluates: _NI128_SHL: attempt to sh
- consts/ptr_comparisons.rs: blaze fails where rustc evaluates: _: MirEvalError(UndefinedBehavior("`unreachable` intrinsic executed")
- consts/recursive-static-write.rs: rustc fails where blaze evaluates: S: encountered static that tried to access; rustc fails where blaze evaluates: S2: encountered static that tried to access
- consts/slice-index-overflow-issue-130284.rs: rustc fails where blaze evaluates: C: overflowing pointer arithmetic: the total offset
- consts/static_mut_containing_mut_ref2.rs: rustc fails where blaze evaluates: STDERR_BUFFER: modifying a static's initial value from
- consts/static_mut_containing_mut_ref3.rs: rustc fails where blaze evaluates: BAR: modifying a static's initial value from
- consts/trait_alias_method_call.rs: blaze fails where rustc evaluates: _: MirLowerError(UnresolvedMethod("bar"))
- consts/write-to-static-mut-in-static.rs: rustc fails where blaze evaluates: B: modifying a static's initial value from; rustc fails where blaze evaluates: C: encountered static that tried to access
- consts/const-eval/assign-to-static-within-other-static.rs: rustc fails where blaze evaluates: BOO: modifying a static's initial value from
- consts/const-eval/atomic.rs: blaze fails where rustc evaluates: ATOMIC_INT: MirEvalError(InternalError("atomic intrinsic arg2 is not provided"); blaze fails where rustc evaluates: ATOMIC_PTR: MirEvalError(InternalError("atomic intrinsic arg1 is not provided"); blaze fails where rustc evaluates: BOOL: MirEvalError(InternalError("atomic intrinsic arg2 is not provided"); blaze fails where rustc evaluates: FENCES: MirEvalError(In
- consts/const-eval/issue-49296.rs: rustc fails where blaze evaluates: X: memory access failed: ALLOC$ID has been
- consts/const-eval/issue-91827-extern-types-field-offset.rs: blaze fails where rustc evaluates: NEWTYPE: MirEvalError(UndefinedBehavior("out of bound memory write")
- consts/const-eval/mod-static-with-const-fn.rs: rustc fails where blaze evaluates: BAR: modifying a static's initial value from
- consts/const-eval/ptr_fragments_mixed.rs: rustc fails where blaze evaluates: APTR: unable to read parts of a; rustc fails where blaze evaluates: BPTR: unable to read parts of a
- consts/const-eval/raw-pointer-ub.rs: rustc fails where blaze evaluates: MISALIGNED_COPY: accessing memory with alignment N, but; rustc fails where blaze evaluates: MISALIGNED_FIELD: accessing memory based on pointer with; rustc fails where blaze evaluates: MISALIGNED_LOAD: accessing memory based on pointer with; rustc fails where blaze evaluates: MISALIGNED_STORE: accessing memory based on pointer with; rustc fails where blaze evalua
- consts/const-eval/read_partial_ptr.rs: rustc fails where blaze evaluates: PTR_BYTES_MIX: unable to read parts of a
- consts/const-eval/ub-enum-overwrite.rs: rustc fails where blaze evaluates: _: reading memory at ALLOC$ID[0x1..0x2], but memory
- consts/const-eval/ub-invalid-values.rs: rustc fails where blaze evaluates: _: interpreting an invalid N-bit value as
- consts/const-eval/ub-slice-get-unchecked.rs: rustc fails where blaze evaluates: ZST_RANGE_OOB: evaluation panicked: unsafe precondition(s) violated: slice::get_unchecked
- consts/const-eval/ub-write-through-immutable.rs: rustc fails where blaze evaluates: WRITE_AFTER_CAST: writing through a pointer that was; rustc fails where blaze evaluates: WRITE_AFTER_TRANSMUTE: writing through a pointer that was
- consts/const-eval/union-ice.rs: rustc fails where blaze evaluates: FIELD3: reading memory at ALLOC$ID[0x0..0x8], but memory; rustc fails where blaze evaluates: FIELD_PATH: reading memory at ALLOC$ID[0x0..0x8], but memory; rustc fails where blaze evaluates: FIELD_PATH2: reading memory at ALLOC$ID[0x0..0x8], but memory
- consts/const-eval/validate_uninhabited_zsts.rs: rustc fails where blaze evaluates: BAR: constructing invalid value of type empty::Empty:
- consts/const-eval/volatile.rs: blaze fails where rustc evaluates: _WRITE: MirEvalError(NotSupported("intrinsic volatile_store is not implemented")
- consts/const-eval/heap/dealloc_intrinsic_duplicate.rs: rustc fails where blaze evaluates: _X: memory access failed: ALLOC$ID has been
- consts/const-eval/heap/dealloc_intrinsic_incorrect_layout.rs: rustc fails where blaze evaluates: _W: invalid align passed to `_`: N; rustc fails where blaze evaluates: _X: incorrect layout on deallocation: ALLOC$ID has; rustc fails where blaze evaluates: _Y: incorrect layout on deallocation: ALLOC$ID has; rustc fails where blaze evaluates: _Z: incorrect layout on deallocation: ALLOC$ID has
- consts/const-eval/heap/make-global-other.rs: rustc fails where blaze evaluates: Y: pointer passed to `_` does not
- consts/const-eval/heap/make-global-twice.rs: rustc fails where blaze evaluates: Y: attempting to call `_` twice on
- consts/const-eval/heap/ptr_made_global_mutated.rs: rustc fails where blaze evaluates: A: writing to ALLOC$ID which is read-only
- consts/const-eval/simd/simd-packed-non-pow2.rs: blaze fails where rustc evaluates: _: MirEvalError(NotSupported("unknown simd intrinsic masked_store")
- const-generics/gca/basic-different-definitions.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:; blaze fails where rustc evaluates: ARR: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented")); blaze fails where rustc evaluates: INC: MirLowerError(UnresolvedName { owner: Body(ConstId(I
- const-generics/gca/basic.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:; blaze fails where rustc evaluates: ARR: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented")); blaze fails where rustc evaluates: INC: MirLowerError(NotSupported("macro call that failed t
- const-generics/gca/coherence-ok.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/gca/generic-free-const.rs: blaze fails where rustc evaluates: ADD1: MirLowerError(UnresolvedName { owner: Body(ConstId(Id)), path: BarePath(ModPath { kind: Plain, segments: [Name { symbol:
- const-generics/mgca/adt_expr_infers_from_value.rs: blaze fails where rustc evaluates: PC: MirLowerError(NotSupported("monomorphization resulted in errors: PC<{type error}, {const error}>")); blaze fails where rustc evaluates: WRAP: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/generic-args-on-enum-variant-segments.rs: blaze fails where rustc evaluates: _: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/paren.rs: blaze fails where rustc evaluates: A: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/type-const-used-in-trait.rs: blaze fails where rustc evaluates: N: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/type_const-generic-param-in-type.rs#gate: blaze fails where rustc evaluates: BAR: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented")); blaze fails where rustc evaluates: BAZ: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented")); blaze fails where rustc evaluates: FOO: MirLowerError(NotSupported("macro call that failed to expand: this bui
- const-generics/mgca/type_const-incemental-compile.rs: blaze fails where rustc evaluates: TYPE_CONST: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/type_const-pub.rs: blaze fails where rustc evaluates: TYPE_CONST: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/type_const-use.rs: blaze fails where rustc evaluates: CONST: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
- const-generics/mgca/type_const_in_pattern.rs: blaze fails where rustc evaluates: CONST: MirLowerError(NotSupported("macro call that failed to expand: this built-in macro is not implemented"))
