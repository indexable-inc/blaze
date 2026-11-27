---
status: current
summary: What `blaze gc` does: the store's byte budget and its two settings, compaction and root eviction in the plain store, removal of other-form stores and stale side files, and the background run a command starts after its answer.
---
# Collecting what blaze keeps on disk

`blaze gc [--max-gb <N> | --max-bytes <N>] [--dry-run] [--all-formats] [--background]`
(`crates/blaze-cli/src/gc.rs`) keeps the home's plain store within a budget.

## The store and the budget

The store is one folder per form, `~/.blaze/v2/store/f<FORMAT>s<SEGMENT>/`: `f6s3` today
(`FORMAT` and `SEGMENT_FORMAT` in `crates/blaze-cas/src/plain/format.rs`). Every worktree on the
machine shares it.

The budget defaults to 12 GiB (`DEFAULT_MAX_STORE_BYTES`). `--max-gb` or `--max-bytes` sets it for
one run; otherwise `BLAZE_STORE_MAX_BYTES` is read, then `BLAZE_STORE_MAX_GB`. `0` means unlimited.
Any other value is raised to at least 1 GiB, and a value that does not parse is an error. Eviction
starts once committed bytes pass the high watermark, 85% of the budget, and goes down to the low
watermark, 75%.

## What one run does, in order

1. It takes `~/.blaze/v2/store/gc.lock` without waiting. If another `blaze gc` holds it, the run
   says so and stops (`--background` stops silently).
2. It opens the store once and reclaims leases left by dead processes.
3. It compacts and sweeps segments and evicts the least recently used roots, at most 1024 slices.
   A slice that aborts on a concurrent commit is retried on the refreshed manifest, up to 8 times.
4. It unlinks retired files whose 60-second grace has passed (`GRACE_NS`).
5. It seals unsealed local segments, so that an open after a reboot checks them without rehashing.
6. It removes other-form store folders, and loose format-4 files directly in `v2/store/`, that
   have gone unused for 3 days (`EXPIRY_DAYS` in `crates/blaze-cli/src/store/forms.rs`).
   `--all-formats` removes them all now. This blaze's own folder is never removed.
7. It sweeps stale side files: `v2/runs/`, `v2/test/tmp/`, `v2/run/records/`, `v2/native-work/`,
   `v2/proc-macro/{compiled,expansions}`, `v2/build-script/{compiled,records,files}`,
   `v2/sdk-answers/` and superseded packs in `v2/toolchain/`. With an unlimited budget these go by
   age only.

`--dry-run` takes no lock and deletes nothing. It prints the store's total, live and reclaimable
bytes, the watermarks, the counts of segments, roots and retired files, and which other-form
folders would go.

## In the background

After a command prints its answer, it commits the store and starts `blaze gc --background` in a
new session (`after_answer_printed` in `crates/blaze-cli/src/store.rs`). It starts one only when
work is due and `gc.lock` is free. Work is due when the store is over budget, a root has expired,
retired files can be unlinked or a tier is full. It is also due when 8 segments or 16 MiB are
unsealed. The command never waits for the child. The child lowers its priority: background QoS on
macOS, nice 19 and idle IO on Linux.

`blaze clean --store` (`crates/blaze-cli/src/clean.rs`) removes the whole store.
