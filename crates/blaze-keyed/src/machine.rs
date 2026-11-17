//! The machine descriptor: one code path, sized by what the machine is (no "Mac mode").

#![blaze::sans_io]

/// What the machine is, read once at start by the driver's host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Machine {
    /// Cores the process may use.
    pub cores: u32,
    /// Performance cores, where the OS tells them apart (Apple silicon); else `cores`.
    pub fast_cores: u32,
    pub page_bytes: u32,
    /// Memory the machine has, not what is free.
    pub memory_bytes: u64,
    /// Whether the OS has QoS classes ([`Qos`]) a thread can be put in.
    pub qos: bool,
    /// Whether huge pages can back large mapped images.
    pub huge_pages: bool,
}

/// The QoS class a thread runs at, where the OS has them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Qos {
    /// The edit path: a no-op, an edit's few tasks.
    Interactive,
    /// A cold build's fan-out: every core.
    Build,
    /// Compaction and pruning.
    Utility,
}

/// Below this many queued tasks the work runs on the main thread: a no-op or a one-body edit
/// starts no worker.
pub const MAIN_THREAD_TASKS: u32 = 2;

/// A batch of tasks is sized to take at least this long, so scheduling stays under 1% of task
/// time (tasks are 0.1 to 5 ms; a dequeue and a steal cost about 1 us).
pub const BATCH_NS: u64 = 200_000;

impl Machine {
    /// Workers to start for `queued` tasks: in proportion to the work, never more than the
    /// cores, none below [`MAIN_THREAD_TASKS`].
    pub fn workers_for(&self, queued: u32) -> u32 {
        match queued < MAIN_THREAD_TASKS {
            true => 0,
            false => queued.min(self.cores.max(1)),
        }
    }

    /// The cap on one task's frame: a small share of memory per worker, so peak memory is
    /// workers times this, and a 16 GB machine running an editor and a browser keeps room.
    pub fn frame_cap_bytes(&self) -> u64 {
        let share = self.memory_bytes / 8 / u64::from(self.cores.max(1));
        share.clamp(16 << 20, 512 << 20)
    }

    /// How many of a file's bodies go in one task, from the mean time of one body.
    pub fn batch_of(&self, mean_body_ns: u64) -> u32 {
        u32::try_from(BATCH_NS / mean_body_ns.max(1)).unwrap_or(u32::MAX).max(1)
    }

    /// The QoS a phase runs at, or `None` where the OS has no classes.
    pub fn qos_of(&self, qos: Qos) -> Option<Qos> {
        self.qos.then_some(qos)
    }
}
