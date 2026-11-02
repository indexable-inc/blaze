---
status: plan
summary: blaze v2: the words the plan and the code use, one line each.
date: 2026-10-29
---
# Glossary

- **View**: a root of the Merkle VFS (every file to its content hash), named by a head. A
  branch, a PR, a CI job and a what-if are each a view.
- **Head**: a name that points at a view's root and moves when it is committed to.
- **Content ID**: a BLAKE3 hash, domain-separated by kind; the identity of anything stored,
  hashed or keyed. Deterministic.
- **Dense ID** (`Idx<T>`): a `u32` index into one arena, assigned in arrival order. Fast,
  never in a hash, key or output.
- **Fingerprint**: 128 bits of a query's result; equal fingerprints stop a rerun's
  propagation (early cutoff).
- **Query**: a pure function of its key and what it reads, memoized by the database.
- **Memo**: a query's stored result, its fingerprint and its edges. The database's only.
- **Edge**: one read a query made: of a file, another query or a fact.
- **Fact**: the narrowest thing a query read (a signature, a layout, one field, a method
  lookup, an impl question, a name in a scope), with its own hash.
- **Negative fact**: an answer of "nothing" (no impl, no name), kept in an index so an added
  impl or item finds the queries it changes.
- **Item hashes**: an item's interface, layout, code and deep hashes (items.md).
- **Plane**: the scheduler: requests, tasks, fair share, critical path, single flight, memory
  admission. Holds no memo.
- **Request**: one build, test, run or edit, with a weight for fair share.
- **Task**: one query execution (or a batch of tiny ones) on the worker pool.
- **Single flight**: tasks with the same key while one runs join it.
- **Admission**: a task starts only when its estimated memory fits the budget.
- **Unit**: the unit of codegen, one function or a batch of small ones.
- **Instance**: a monomorphized function, keyed by its generic's code hash and the facts of
  its type arguments it reads.
- **Compiled function**: an instance's machine code, relocations, unwind and line tables.
- **Image**: the daemon's whole database as one zero-copy file, mapped on start.
- **Base layer**: compiled public dependencies and std, read-only, shared by every project.
- **Oracle**: the check that an incremental result equals a from-scratch build.
- **Checkpoint**: the coordinator's hourly commit, manifest lint and main merge. Nothing is
  compiled at a checkpoint; the first compile waits until every lane is done.
