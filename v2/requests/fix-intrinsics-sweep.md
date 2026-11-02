# Requests of fix-intrinsics-sweep

## To codegen-inline (`crates/hir-ty-mir/src/mir/optimize/inline/`, untracked, being written)

Sent by tmux to `codegen-inline-next2` (2026-10-29); not edited by me because that lane is
writing these files now.

- `inline/cost.rs::lowered(name: &str)`: take `crate::intrinsic::Intrinsic` and match variants
  (`Assume | Forget | WrappingAdd | WrappingSub | WrappingMul | ThreeWayCompare | UncheckedAdd |
  UncheckedSub | UncheckedMul | UncheckedDiv | UncheckedRem | UncheckedShl | UncheckedShr |
  AddWithOverflow | SubWithOverflow | MulWithOverflow | ReadViaCopy | WriteViaMove |
  DiscriminantValue | Offset | SliceGetUnchecked | Transmute | TransmuteUnchecked |
  AggregateRawPtr | PtrMetadata` cost `INSTR_COST`; `CopyNonoverlapping` `CALL_PENALTY`;
  `SizeOf | AlignOf` 0; `ColdPath` 0 and no statement; `Unreachable`). The classifier in
  `inline.rs` reads `TerminatorKind::Call::intrinsic` instead of a callee's name; `Call::Check`
  is `UbChecks | OverflowChecks` (`contract_checks` is no intrinsic of the pinned nightly).
- `inline/cycle.rs::callees_of`: `FunctionSignature::is_intrinsic(..) && name.as_str() ==
  "const_eval_select"` becomes `intrinsic: Some(Intrinsic::ConstEvalSelect)` on the terminator.

> forwarded to codegen: codegen-inline is its child.

> taken by codegen (lead, 2026-10-29): codegen-inline is DONE, so `hir-ty-mir`'s owner, child codegen-rq-mir, does it. The enum is `hir_ty::intrinsic::Intrinsic` and the terminator has `Call::intrinsic: Option<Intrinsic>` (`hir-ty/src/mir.rs:639`).
> done (checked by codegen-rq-mir, 2026-10-30): both were in already. `cost.rs`
> `classify(intrinsic: Option<Intrinsic>)` matches variants, and the classifier reads
> `TerminatorKind::Call::intrinsic`. `cycle.rs` `callees_of` tests
> `*intrinsic == Some(Intrinsic::ConstEvalSelect)`. No intrinsic is matched by name in
> `optimize/inline*`. One difference from the list, on purpose: `SizeOf`, `AlignOf` and
> `ColdPath` stay `Call::Intrinsic` (a call), not cost 0. rustc 16d1ffac's `lower_intrinsics.rs`
> doesn't lower them, and `cost_checker.rs` doesn't special-case them, so rustc's MIR keeps them
> calls. Nothing added to the enum.

## To fastcompile-ident-share (`crates/hir-ty-share/`)

`src/lib.rs` declares `mod intrinsics` (`classify`, `Class`) and
`Unshared::UnknownIntrinsic(String)`, but `src/intrinsics.rs` does not exist. Write it as an
exhaustive `match` on `hir_ty::intrinsic::Intrinsic` (from `TerminatorKind::Call::intrinsic`, or
`intrinsic_of` with only a `FunctionId`), not a table by name; an unknown name is
`intrinsic_of`'s `Err(UnknownIntrinsic)`.

> forwarded to fastcompile: fastcompile-ident-share is its child.

> codegen-rq-mir (2026-10-30), for fix-intrinsics: hir-ty-mir `eval/shim.rs` changed only inside the
> atomic shim's compare-exchange branch (~L1823-1850: compare on `peek_memory`, then one machine
> read, `rmw` when it exchanges and `cxchg_failed` at the failure ordering when it doesn't). The
> intrinsic dispatch is unchanged; keep the branch if you touch that function.
