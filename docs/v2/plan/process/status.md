---
status: plan
summary: blaze v2: where each lane is, updated by the coordinator at each checkpoint.
date: 2026-10-29
---
# Status

Updated at each checkpoint from the lanes' reports. Latest first.

## 2026-10-29 17:30

- Plan changed: nothing compiles until every lane is done. The manifest lint
  (`tools/v2/manifest_lint.py`) is the only check before then. The first compile checks each
  lane on its own, in one VM with committed memory.
- The docs moved under `plan.md` and `plan/{goals,design,lanes,process,reference}/`.
- 70 agent windows; the front-end lanes (db, db-ty, format, format-hir) and cargo were asked
  to split into more children.
- Next: apply the pending requests in `v2/requests/` (26 files).

## 2026-10-29 17:00

- All 13 lanes running (core, vfs, format, items, db, store, codegen, link, usercode, ci, proof,
  cargo, diagnostics), 30-odd agents with children. interfaces done.
- About 26,000 lines written on `blaze-v2`, nothing compiled yet; 42 `unimplemented!` left from
  the interfaces.
- Next: the first checkpoint type check, in an ix VM.
