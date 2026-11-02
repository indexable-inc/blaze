# Requests from db-query2 (lane db; `crates/blaze-query/`)

## 1. To core (`crates/blaze-graph/src/pump.rs`): `Flights::run_if_idle`, a run that never waits

What: one method on `Flights`, beside `get_or_run`:

```rust
    /// As [`Flights::get_or_run`], but never waits: `None`, running nothing, when another task
    /// runs `key` now.
    pub fn run_if_idle(
        &self,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Option<Result<V, Cancelled>> {
        let me = current();
        match self.shard(&key).lock().entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight { runner: me, waiters: Waiters(ptr::null()) });
            }
            Entry::Occupied(slot) if slot.get().runner == me => {
                panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key))
            }
            Entry::Occupied(_) => return None,
        }
        let mut ending = Ending { flights: self, key, runner: me, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        Some(result)
    }
```

Why: `Cx::get_all` (cargo's request 20 in `v2/requests/cargo.md`) resolves the keys after the
first as jobs of the process pool (`exec::shared().scope`), while the asking task asks every key
in order. The asker's wait for the scope's jobs is no edge of the wait graph. So a job that
waited for a key, through a chain leading back to a key the asker holds, would deadlock unseen.
A job ahead therefore never waits. It runs a key only when no one runs it, and otherwise gives
way at once, leaving the key for the asker to ask in its turn. The asker's own waits are the
usual visible ones. `blaze-query`'s `Database::single` calls this for a run ahead
(`crates/blaze-query/src/lib.rs`). It doesn't compile until this is in.

Status: open

> applied (requests, pass 31; lane core is DONE): `Flights::run_if_idle` after `get_or_run` in `crates/blaze-graph/src/pump.rs`, as written, with the test `a_run_if_idle_never_waits`: it runs a free key, gives way at once without the wait graph's lock to a key another thread runs, and runs that key once it is free.
