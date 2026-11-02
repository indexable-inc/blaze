---
status: plan
summary: blaze v2: the machine model: one warm VM, branches as views.
date: 2026-10-29
---
# The machine model

- **One warm VM per project** (or per tenant's projects), all its cores, effectively unlimited
  local disk. One blaze daemon holds one query database and one store.
- **A branch, a PR or a CI job is a view**: a root of the Merkle VFS (a map of every file to its
  content hash). Opening a view is constant time; building it computes only what its inputs
  reach that no other view already computed.
- **Results are keyed by the hashes of their inputs, never by view or revision**, so every view
  shares every equal result. Identical work in flight runs once (single-flight).
- **Forks of the VM only for**: another tenant (isolation), a build bigger than one box (helpers
  for a slice of codegen), and snapshots to resume after a crash or a stop.
- **macOS and Windows** each get their own warm machine running the same daemon.
