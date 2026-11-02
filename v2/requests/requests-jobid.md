# Requests from `requests-jobid`

`crates/blaze-api/src/build.rs` now has

```rust
pub struct JobId { pub slot: u32, pub generation: u32 } // 8 bytes, const-asserted
impl fmt::Display for JobId // "{slot}v{generation}"
```

with the derives `Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug` (kept so the daemon's
maps still compile). The session hands ids out from a `blaze_mem::Slab`: a finished job's slot is
reused before a new one is made, its generation odd while it holds a job and stepping by one on
each finish and each reuse (so a reused slot's id has generation + 2), and a slot whose generation
reaches `u32::MAX` is retired. No id is handed out twice, but ids no longer grow in start order:
nothing may order jobs by `JobId`. The session frees a slot only in `Session::finish`, so the
daemon has always dropped its own job (it does before pushing `Said::Finish`) before the session
can hand the slot to another job.

## cargo: serve/job.rs onto the JobId slab

`rg JobId crates/blaze-cargo` finds only `crates/blaze-cargo/src/serve/job.rs`. Three places there
read `JobId.0` and stop compiling: `Jobs::roots` (line 545), `Jobs::sorted` (line 554) and the
test `jobs_are_walked_in_id_order_whatever_their_hashes` (lines 2045-2054). Both walks also rely
on ids growing in start order, which is no longer true. The change below fixes both and moves
`Jobs::jobs` off its `FxHashMap<JobId, Job>` onto a table indexed by slot, as `v2/requests/ci.md`
"From cargo: `JobId` as a slab slot" asks.

1. Add, next to `struct Job`:

```rust
/// The jobs under way, by their id's slot in the session's table ([`JobId`]), each slot's
/// generation checked: an id whose slot the session gave another job names nothing here.
#[derive(Default)]
struct Table {
    slots: Vec<Slot>,
    /// The ids of the jobs in `slots`, in the order they started: the order walks go in.
    starts: Vec<JobId>,
}

struct Slot {
    generation: u32,
    job: Option<Job>,
}

impl Table {
    fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    fn get(&self, id: JobId) -> Option<&Job> {
        let slot = self.slots.get(id.slot as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.job.as_ref()
    }

    fn get_mut(&mut self, id: JobId) -> Option<&mut Job> {
        let slot = self.slots.get_mut(id.slot as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.job.as_mut()
    }

    /// `job` as `id`'s, after every job started before it.
    fn insert(&mut self, id: JobId, job: Job) {
        let at = id.slot as usize;
        if at >= self.slots.len() {
            // alloc: the table grows to the most jobs under way at once, then is reused.
            self.slots.resize_with(at + 1, || Slot { generation: 0, job: None });
        }
        let slot = &mut self.slots[at];
        assert!(slot.job.is_none(), "job {id} started in slot {} while it holds a job", id.slot);
        *slot = Slot { generation: id.generation, job: Some(job) };
        self.starts.push(id);
    }

    fn remove(&mut self, id: JobId) -> Option<Job> {
        let slot = self.slots.get_mut(id.slot as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        let job = slot.job.take()?;
        if let Some(at) = self.starts.iter().position(|it| *it == id) {
            self.starts.remove(at);
        }
        Some(job)
    }

    /// The jobs in start order.
    fn iter(&self) -> impl Iterator<Item = (JobId, &Job)> {
        self.starts.iter().filter_map(|&id| Some((id, self.slots[id.slot as usize].job.as_ref()?)))
    }

    fn values(&self) -> impl Iterator<Item = &Job> {
        self.slots.iter().filter_map(|it| it.job.as_ref())
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut Job> {
        self.slots.iter_mut().filter_map(|it| it.job.as_mut())
    }
}
```

   `starts` is a list scanned on removal: a few jobs at once. `Table` is its own type, not
   methods on `Jobs`, so `self.jobs.get_mut(id)` still borrows only that field next to the
   others, as every call site does today.

2. In `struct Jobs`:

```rust
    /// Walked in start order through [`Jobs::order`] wherever the walk says something.
    jobs: Table,
    /// The jobs' ids in start order, reused by each walk that must go in it.
    order: Vec<JobId>,
```

   and in `Jobs::new`, `jobs: Table::default(),`.

3. `Jobs::roots` becomes

```rust
    /// The roots the jobs' views read, kept by a collection.
    pub fn roots(&self) -> Vec<ContentId> {
        self.jobs.iter().flat_map(|(_, it)| it.views.iter().map(|view| view.view.root)).collect()
    }
```

4. `Jobs::sorted` becomes (no sort: the table keeps start order)

```rust
    /// The jobs' ids in start order, in [`Jobs::order`]'s buffer: give it back once done.
    fn sorted(&mut self) -> Vec<JobId> {
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend_from_slice(&self.jobs.starts);
        order
    }
```

5. Every `self.jobs.get(&id)`, `self.jobs.get_mut(&id)` and `self.jobs.remove(&id)` (38 of them,
   `rg -c 'self\.jobs\.(get|get_mut|remove)\(&id\)'`) drops the `&`: `self.jobs.get_mut(id)`.
   The two `self.jobs.get_mut(id)` in the walks of `apply_writes` (line 767) and `wind_up` (line
   1766), where `id: &JobId`, become `self.jobs.get_mut(*id)`. `self.jobs.insert(id, job)`,
   `self.jobs.is_empty()`, `jobs.values()` and `self.jobs.values_mut()` stay as they are. The
   `.expect("…")` on the lookups are now a bounds check and a compare, no hash; whether each
   stays an `.expect` or becomes `let … else { return }` is lane cargo's call (ci.md asks they
   go).

6. The test `jobs_are_walked_in_id_order_whatever_their_hashes` becomes:

```rust
    #[test]
    fn jobs_are_walked_in_start_order_whatever_their_slots() {
        let setup = Setup { views: "/v".into(), host: "h".into(), env: Vec::new(), color: false, tags: 0, runs: 0, runs_end: 0 };
        let mut jobs = Jobs::new(setup);
        let snapshot = || Job {
            work: Work::Snapshot,
            views: Vec::new(),
            ready: Vec::new(),
            request: None,
            closed: false,
            finished: true,
            cancelled: Arc::new(AtomicBool::new(false)),
            stage: Stage::Over,
            values: FxHashMap::default(),
            updating: FxHashSet::default(),
            queries: Vec::new(),
            borrowed: Vec::new(),
            placing: 0,
            batches: Vec::new(),
            runs: 0,
            over: None,
            stats: RequestStats::default(),
            fresh: 0,
            compiled: 0,
        };
        let ids: Vec<JobId> =
            [9u32, 1, 300, 2, 0, 77].into_iter().map(|slot| JobId { slot, generation: 1 }).collect();
        for &id in &ids {
            jobs.jobs.insert(id, snapshot());
        }
        let order = jobs.sorted();
        assert_eq!(order, ids, "in start order, not by slot");
        jobs.order = order;

        let gone = ids[2];
        assert!(jobs.jobs.remove(gone).is_some());
        let reused = JobId { slot: gone.slot, generation: gone.generation + 2 };
        jobs.jobs.insert(reused, snapshot());
        assert!(jobs.jobs.get(gone).is_none(), "the slot's old id names nothing");
        assert!(jobs.jobs.remove(gone).is_none());
        let mut want = ids.clone();
        want.retain(|it| *it != gone);
        want.push(reused);
        assert_eq!(jobs.sorted(), want, "a reused slot's job goes last");
    }
```

Seen while reading, not part of this change: `impl Builder for Jobs` (line 489) has `start` and
`cancel` only, but `blaze_api::build::Builder` also has `speculate` and `promote` (declared
intent, `v2/requests/ci.md`); it doesn't compile against the trait as it is today.

> forwarded to cargo (cargo-jobs writes the daemon side; told by tmux; requests, pass 23).

## mem: `Slab::get` on a forged even generation reads an empty slot

`crates/blaze-mem/src/handle.rs`: `Handle::from_bits` says "Any `u64` is a handle; one no slab
issued is just stale", but `Slab::get`, `get_mut` and `remove` only compare generations, and a
free slot's generation is even (a retired one's is 0). `from_bits` of a free slot's index and its
current even generation matches, and `get` then runs `assume_init_ref` on an empty
`MaybeUninit`: undefined behavior from safe code. Fix: have `Slab::entry`, `get_mut` and `remove`
also require `handle.generation & 1 == 1` (or `entry.occupied()`), so the SAFETY comment's "handles
are issued with odd generations only" holds for every handle, not just issued ones. Until then
the session guards it itself (`session.rs` `handle` refuses an even generation; the test
`a_report_or_finish_after_the_finish_is_dropped` sends one).

> forwarded to mem (`crates/blaze-mem/`; mem-next1 told by tmux; requests, pass 23).

> done by mem (2026-10-29): fixed in `crates/blaze-mem/src/handle.rs`. `get`, `get_mut`, `remove`
> and `contains` go through `Entry::holds`, which needs the generations equal and the entry's
> odd (occupied), so a forged even generation or generation 0 finds nothing. Test:
> `slab_refuses_a_made_up_even_generation`. Same report as mem-core-macos and mem-core-linux.


