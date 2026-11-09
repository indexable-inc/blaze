//! The one schema of what blaze records about itself: every reader (the Parquet history,
//! `blaze debug sql`, `blaze debug live`, the explorer at ix.dev/explore, the stall reports)
//! reads these tables, and nothing else defines them. The Arrow schema of each table is made
//! from it (`arrow.rs`), and so are the DuckDB views and the table of columns in
//! docs/profiling.md.
//!
//! Time: every `*_ns` instant is nanoseconds on the machine's monotonic clock ([`CLOCK`]): the
//! same clock in every process of the machine, so the spans of the client, the daemon and the
//! build scripts line up without adjusting. Durations are nanoseconds.

use crate::span;

/// Bumped when a column changes meaning or is removed; adding a nullable column at the end of
/// a table does not bump it. At 2, among others: `spans.owner` names a job by its generation
/// << 32 | its slot (was slot + 1), and so does `switches.wait_value` of a park on a job (was
/// its slot). At 3: `spans` no longer holds every span. Spans shorter than
/// `flight::FOLD_BELOW` (waits shorter than `flight::WAIT_FOLD_BELOW`) are summed into
/// `folded` rows a parent's site and site, and a span's self time is its duration less its
/// children's rows and its `folded_ns`. A file of version 2 or older has no `folded_ns` column
/// and folded nothing (no blaze before 3 turned folding on), so a null `folded_ns` reads as
/// 0 and its spans' self time stays right. At 4: `provenance` is the query engine's run log
/// (`blaze_query::db::take_log_in`), one row a run that was not a first and one an untracked read,
/// with the read that changed (`cause_key`, `cause_kind`) or the untracked read's name (`what`)
/// and the database view; its never-written `dur_ns`, `unit` and `crate` are gone, and
/// `outcome` says `ran` or `untracked read` (no file before 4 has a `provenance` row). At 5: `events.message` is the message alone (it
/// was the message, then each other field as `name=value`); an event's other fields are rows of
/// `event_fields`, joined on `process` and `events.event_id`. A file of version 4 or older has no
/// `event_id` and its fields are in its `message`.
pub const VERSION: u32 = 5;

/// A version before [`VERSION`] whose files a reader reads as [`VERSION`]'s, and what reading
/// them so takes.
#[derive(Debug)]
pub struct Upgrade {
    pub from: u32,
    /// What changes between `from` and the next version, and why reading as the next is right.
    pub how: &'static str,
}

/// Each older version a reader reads, oldest last, with no rewrite. 4 reads as 5: its `events`
/// have no `event_id`, which the views read through the table's schema file as null, and the
/// loaders take a null `event_id` as an event with no `event_fields` rows; its fields stay in its
/// `message` as `name=value`, as 4 wrote them. 3 reads as 5 as 4 does, and its `query_counts`
/// have none of 4's `written` and `ran_*` columns, read as null (not counted, as each column
/// says); it has no `provenance` row (no blaze before 4 wrote one) and no `key_parts`. 2 reads as
/// 5 as 3 does and also has no `folded_ns`, read as null, which every reader takes as 0, right
/// as no blaze before 3 folded. 1 does not: its `spans.owner` and `switches.wait_value` name a
/// job by its slot + 1, and its generation, which 2's `generation << 32 | slot` needs, was never
/// written.
pub const UPGRADES: &[Upgrade] = &[
    Upgrade {
        from: 4,
        how: "no `events.event_id` column: read through the schema file as null, an event with no `event_fields` rows; its fields stay in its `message`",
    },
    Upgrade {
        from: 3,
        how: "as 4, and no `query_counts` `written` or `ran_*` columns (read as null: not counted), no `provenance` rows and no `key_parts` table",
    },
    Upgrade {
        from: 2,
        how: "as 3, and no `folded_ns` column: read through the schema file as null, which reads as 0; no blaze before 3 folded",
    },
];

/// Whether a file of `version` reads as [`VERSION`]'s: `Err` says why not, for the reader's
/// refusal naming the file.
pub fn readable(version: u32) -> Result<(), &'static str> {
    if version == VERSION || UPGRADES.iter().any(|it| it.from == version) {
        return Ok(());
    }
    if version > VERSION {
        return Err(
            "it is of a newer schema than this blaze reads: read it with the blaze that wrote it",
        );
    }
    Err(
        "it is of a schema this blaze has no upgrade from: its rows mean something no reader can say (version 1 named jobs without their generation); delete it, or the history",
    )
}

/// The name of the clock the `*_ns` instants count on, as `hello` says it.
#[cfg(target_os = "linux")]
pub const CLOCK: &str = "linux CLOCK_MONOTONIC";
#[cfg(target_vendor = "apple")]
pub const CLOCK: &str = "darwin CLOCK_UPTIME_RAW";
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub const CLOCK: &str = "std Instant";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    U16,
    U32,
    U64,
    I64,
    F64,
    Bool,
    Utf8,
    /// A string from a small set (names, kinds): dictionary-encoded, `u32` keys.
    Dict,
    /// A call stack, outermost frame first, each frame a [`Type::Dict`] string.
    Stack,
    /// A call stack as raw return addresses, outermost first, each a `u64`: what the image the
    /// row names (its hash and load base) can name again later.
    Pcs,
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub name: &'static str,
    pub ty: Type,
    pub nullable: bool,
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Table {
    pub name: &'static str,
    pub doc: &'static str,
    pub columns: &'static [Column],
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|it| it.name == name)
    }
}

const fn col(name: &'static str, ty: Type, doc: &'static str) -> Column {
    Column { name, ty, nullable: false, doc }
}

const fn opt(name: &'static str, ty: Type, doc: &'static str) -> Column {
    Column { name, ty, nullable: true, doc }
}

const BUILD_ID: Column =
    opt("build_id", Type::U64, "the build it was part of; null outside any build");
const PROCESS: Column = col("process", Type::U32, "the process id");
const PROCESS_NAME: Column = col(
    "process_name",
    Type::Dict,
    "what the process is: blaze serve, blaze build, a build script's crate",
);
const THREAD: Column = col("thread", Type::U32, "the operating system's thread id");
const THREAD_NAME: Column = col("thread_name", Type::Dict, "the thread's name");

/// The columns of `spans` before those span fields fill.
const SPAN_HEAD: [Column; 17] = [
    BUILD_ID,
    opt("commit", Type::Dict, "the commit of the workspace the build was of"),
    PROCESS,
    PROCESS_NAME,
    THREAD,
    THREAD_NAME,
    col("span_id", Type::U64, "unique in its process while the process lives"),
    opt("parent", Type::U64, "the span it is inside; null at a root"),
    col("depth", Type::U16, "how many spans its thread was inside when it began"),
    col("name", Type::Dict, "the span's name"),
    col(
        "target",
        Type::Dict,
        "its tracing target: the module path it was made in, unless it names one (`blaze` for blaze's steps)",
    ),
    col("start_ns", Type::I64, "when it began"),
    col("dur_ns", Type::I64, "how long it was open"),
    opt(
        "self_cpu_ns",
        Type::I64,
        "its CPU time while it was the innermost span its thread was inside (its job's alone, less the spans under it; a wait's is about 0, as other jobs run while it parks): the thread's CPU clock is read at switches, waits and gaps between root spans and each stretch's CPU is shared among the spans by the time each was innermost in it, so it is off by at most the off-CPU time of the stretches it ran in (`time::cpu`); null when the binary counts no CPU (`flight::count_cpu`) or a read failed",
    ),
    opt("alloc_bytes", Type::U64, "bytes its thread allocated while it was open"),
    opt("allocs", Type::U64, "allocations its thread made while it was open"),
    opt("query_kind", Type::Dict, "the query it ran, for a span of a query"),
];

/// Beside `alloc_bytes` in meaning, after the generic span columns so adding them kept
/// `VERSION`.
const SPAN_FRAMES: [Column; 2] = [
    opt(
        "frame_grow_copies",
        Type::U64,
        "blocks its thread's task frames moved to grow them while it was open (a block grown while not its frame's last is copied whole); null when not counted or when it closed on another thread",
    ),
    opt("frame_copied_bytes", Type::U64, "bytes those moves copied"),
];

/// What a span's children folded into `folded` rows took of it (`flight::fold_below`), after
/// the frame counts; added at version 3.
const SPAN_FOLDED: [Column; 2] = [
    opt(
        "folded_spans",
        Type::U64,
        "its children too short for a row each (`folded`), summed under its name there; null when none was",
    ),
    opt(
        "folded_ns",
        Type::I64,
        "their durations summed: its self time is dur_ns less its children's rows and this; null when none was",
    ),
];

const SPAN_COLUMNS: usize =
    SPAN_HEAD.len() + span::GENERIC + SPAN_FRAMES.len() + SPAN_FOLDED.len() + span::TYPED;

/// The span columns in their order: the head, the generic columns span kinds and ad hoc spans
/// fill, the frame counts, the folded children, then the typed columns only span kinds fill
/// ([`span`]). A typed column added to the registry goes at the end, so it keeps `VERSION`.
const SPAN_ALL: [Column; SPAN_COLUMNS] = {
    let parts: [&[Column]; 5] =
        [&SPAN_HEAD, &span::GENERIC_COLUMNS, &SPAN_FRAMES, &SPAN_FOLDED, &span::TYPED_COLUMNS];
    let mut out = [SPAN_HEAD[0]; SPAN_COLUMNS];
    let mut at = 0;
    let mut part = 0;
    while part < parts.len() {
        let mut i = 0;
        while i < parts[part].len() {
            out[at] = parts[part][i];
            at += 1;
            i += 1;
        }
        part += 1;
    }
    out
};

/// One row a span: from when it was made to when it closed, on the thread that made it.
pub const SPANS: Table = Table {
    name: "spans",
    doc: "one row a span, from its start to its close, on the thread that made it",
    columns: &SPAN_ALL,
};

/// Where `spans`' generic span columns begin: `batches.rs` writes them there.
pub const SPANS_GENERIC_AT: usize = SPAN_HEAD.len();

/// A value over time: queue depths, memo counts, bytes.
pub const COUNTERS: Table = Table {
    name: "counters",
    doc: "one row a value of a counter at an instant",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("name", Type::Dict, "what is counted"),
        col("t_ns", Type::I64, "when"),
        col("value", Type::F64, "the value then"),
        opt(
            "fiber",
            Type::U32,
            "the fiber a fiber stack counter is of; null for any other counter",
        ),
        opt("job", Type::U64, "the job that ran on that fiber; null for any other counter"),
    ],
};

/// One row a switch of a job on a fiber: started, parked (waiting, or moving to another core),
/// resumed and ended, on the thread it happened on. A wait is a job's `parked` row and its next
/// `resumed` one; when their threads differ, the job moved.
pub const SWITCHES: Table = Table {
    name: "switches",
    doc: "one row a switch of a job on a fiber, on the thread it happened on",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("t_ns", Type::I64, "when"),
        col(
            "event",
            Type::Dict,
            "started (joined the graph), parked, ready (made runnable, on its waker's thread), resumed, ended, ran (a pool job that never joined the graph ended), or a worker's idle and woke",
        ),
        col(
            "job",
            Type::U64,
            "the job: its slot's generation << 32 | its slot, one job of its driver's life; for ran, its number before it joined the graph (slot 0xffffffff); 0 for idle and woke",
        ),
        col("fiber", Type::U32, "the fiber it runs on, numbered in its process"),
        col("core", Type::U32, "the driver's core of the thread"),
        opt(
            "wait_kind",
            Type::Dict,
            "for parked: what it waits for (job, key, counter, until) or moved; null otherwise",
        ),
        opt(
            "wait_value",
            Type::U64,
            "the job (as `job` names it), the key's hash, the counter or the driver-clock deadline; null for moved",
        ),
        opt(
            // Not `by`: an SQL keyword, which every query would have to quote.
            "by_job",
            Type::U64,
            "for started and ran, the job running where it was given to the pool (its spawner); for ready, the job that woke it; null for none (a thread on no fiber) and any other event",
        ),
        opt(
            "since_ns",
            Type::I64,
            "for started and ran, when the job began on its fiber, before it joined the graph",
        ),
        opt(
            "began_as",
            Type::U64,
            "for started, the job's number before it joined the graph, which its spawned jobs' `by_job` and its `job` span name until then",
        ),
        opt(
            "queued_ns",
            Type::I64,
            "for started and ran, how long the job waited for a core: from its pool's submit to `since_ns`; null when the pool did not say when it was given",
        ),
        opt(
            "cpu_ns",
            Type::I64,
            "the thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`) read at the switch, which ends a stretch of `spans.self_cpu_ns`: two of a thread's rows give the CPU it had between them; null for ready, when the binary counts no CPU, and in files before 2026-11-04",
        ),
    ],
};

/// One row a `tracing` event the recorder keeps (`blaze_profile::logs::keeps_event`) and the
/// history writes (`blaze_profile::logs::Hold`), on the thread it happened on. Its fields other
/// than its message are rows of [`EVENT_FIELDS`].
pub const EVENTS: Table = Table {
    name: "events",
    doc: "one row a tracing event kept (error and warn of any target; info of a blaze target; debug of the target blaze, written when its build failed or stalled or the history is set to debug), on the thread it happened on",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("t_ns", Type::I64, "when"),
        opt(
            "job",
            Type::U64,
            "the job running on the thread, as `switches.job` names it (its `began_as` before it joined the graph); null on no job",
        ),
        col("level", Type::Dict, "ERROR, WARN, INFO or DEBUG"),
        col(
            "target",
            Type::Dict,
            "its tracing target: the module path it was written in, unless it names one",
        ),
        col("name", Type::Dict, "its callsite, as tracing names it: `event <file>:<line>`"),
        opt(
            "message",
            Type::Utf8,
            "its message alone (tracing's `message` field); its other fields are rows of `event_fields`; null when it has none or its text was let go before a reader read it",
        ),
        opt("span_id", Type::U64, "the span it happened in, as `spans.span_id`; null outside any"),
        col(
            "event_id",
            Type::U64,
            "unique in its process for the process's life (its thread's ring << 48 | the row's place in that ring): what `event_fields` rows name it by, with `process`",
        ),
    ],
};

/// One row a field of an [`EVENTS`] row other than its message, typed as the event recorded
/// it: a field's key is its callsite's (callsites are static), and its value sits in the one
/// value column its `kind` names, the others null.
pub const EVENT_FIELDS: Table = Table {
    name: "event_fields",
    doc: "one row a field of a kept tracing event other than its message, its value in the column its kind names",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("event_id", Type::U64, "the event, as `events.event_id` names it in its process"),
        col(
            "ordinal",
            Type::U16,
            "the field's place among its event's fields, in the order the event recorded them, from 0",
        ),
        col("key", Type::Dict, "the field's name, as its callsite writes it"),
        col(
            "kind",
            Type::Dict,
            "which value column holds it: i64, u64, f64, bool or str (a `&str`, or anything else recorded through `Display` or `Debug`, as text)",
        ),
        opt("value_i64", Type::I64, "for kind i64: a signed integer (an i128 that fits)"),
        opt("value_u64", Type::U64, "for kind u64: an unsigned integer (a u128 that fits)"),
        opt("value_f64", Type::F64, "for kind f64: a float"),
        opt("value_bool", Type::Bool, "for kind bool"),
        opt(
            "value_str",
            Type::Utf8,
            "for kind str: its text; null when the text was let go before a reader read it",
        ),
    ],
};

/// One row a sum of short spans: those of a site under spans of one site, root or not, that a
/// thread closed in a stretch of up to `flight::FOLD_AGE`, each shorter than
/// `flight::FOLD_BELOW` and named by no other row, saying no typed column or a wait shorter
/// than `flight::WAIT_FOLD_BELOW`, folded instead of written one row each
/// (`flight::fold_below`). Every span folded is counted in one row here, and its parent's
/// `spans` row says how many and how long (`folded_spans`, `folded_ns`).
pub const FOLDED: Table = Table {
    name: "folded",
    doc: "one row a sum of a site's short spans under one parent's site (under 20 us, a wait under 1 us, named by no row) a thread closed in a stretch, kept as a sum instead of a row each",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("name", Type::Dict, "the spans' name, as `spans.name`"),
        col("target", Type::Dict, "their tracing target, as `spans.target`"),
        col(
            "root",
            Type::U32,
            "1 when they were roots (depth 0: a worker's own work between jobs, which `uncovered_per_worker` counts as covered), else 0",
        ),
        col("from_ns", Type::I64, "when the first began"),
        col("to_ns", Type::I64, "when the last closed"),
        col("spans", Type::U64, "how many were folded"),
        col("dur_ns", Type::I64, "their durations summed"),
        col("max_dur_ns", Type::I64, "the longest"),
        opt(
            "self_cpu_ns",
            Type::I64,
            "their self CPU summed, as `spans.self_cpu_ns`; null when not counted",
        ),
        opt("alloc_bytes", Type::U64, "bytes their thread allocated while they were open, summed"),
        opt("allocs", Type::U64, "allocations made while they were open, summed"),
        opt(
            "parent_name",
            Type::Dict,
            "the name of the spans they were inside, as `spans.name`; null for roots and where none was open as they closed",
        ),
        opt("parent_target", Type::Dict, "that parent's tracing target, as `spans.target`"),
    ],
};

/// One row a build request.
pub const BUILDS: Table = Table {
    name: "builds",
    doc: "one row a build request the daemon ran",
    columns: &[
        col(
            "build_id",
            Type::U64,
            "the build's number, unique in the history: a daemon numbers its builds past every build its files hold rows of",
        ),
        opt("commit", Type::Utf8, "the workspace's commit, when it is a git checkout"),
        col("workspace", Type::Utf8, "the workspace's root"),
        col("command", Type::Utf8, "what was asked: build, test, run, check, with the arguments"),
        col("started_ns", Type::I64, "when the daemon took it"),
        opt("dur_ns", Type::I64, "how long it ran; null while it runs"),
        opt("started_unix_ns", Type::I64, "when it was taken, on the wall clock"),
        col(
            "outcome",
            Type::Dict,
            "ok, failed, cancelled, stalled, running or crashed (its daemon died while it ran; written by the postmortem of the daemon's flight recorder)",
        ),
        opt("hits", Type::U64, "nodes the memo settled without running them"),
        opt("misses", Type::U64, "nodes that ran their task"),
        opt("joined", Type::U64, "nodes that took a task another node ran"),
        opt(
            "tree",
            Type::Utf8,
            "the digest of the workspace's source tree when the build ended, as `blaze-v2-proof tree` makes it (target/ and .git/ left out); null for a build of no directory",
        ),
        opt("blaze", Type::Utf8, "the BLAKE3 hash of the blaze executable that ran it"),
        opt(
            "process",
            Type::U32,
            "the daemon's process id: a build's row with outcome running and no row after it is one its daemon never ended, which died if that process no longer writes the history; null in rows of a blaze before it",
        ),
        opt(
            "client_started_ns",
            Type::I64,
            "when the client process that asked for it started, on the history's clock (`started_ns`'s): the client's own start-up before it reached the daemon is `started_ns - client_started_ns`; null until the client sent its rows, and for a build no client asked for",
        ),
        opt(
            "client_ended_ns",
            Type::I64,
            "when that client had the answer and sent its rows, on the same clock: what the client spent after the daemon ended the build is `client_ended_ns - started_ns - dur_ns`",
        ),
        opt(
            "daemon_user_ns",
            Type::I64,
            "the daemon process's user CPU over the build, from getrusage(RUSAGE_SELF) at its start and end: what the OS saw the daemon spend, set beside the spans' `self_cpu_ns` sum to show CPU no span covers. Process-wide, so of two builds that overlap each has the other's share; null while it runs",
        ),
        opt(
            "daemon_system_ns",
            Type::I64,
            "the daemon process's system CPU over the build, as `daemon_user_ns`",
        ),
    ],
};

/// The CPU sampler's samples: where each running thread was, 49 times a second.
pub const SAMPLES: Table = Table {
    name: "samples",
    doc: "one row a CPU sample of a running thread",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        col("t_ns", Type::I64, "when it was taken"),
        col("stack", Type::Stack, "the call stack, outermost first, each frame demangled"),
        opt("span_id", Type::U64, "the innermost span its thread was inside"),
        opt(
            "cpu",
            Type::Dict,
            "`on` when its thread was running or runnable as it was sampled, `off` when blocked inside a span, so what it blocked in shows; the same on Linux and macOS",
        ),
        STACK_PCS,
        opt(
            "job",
            Type::U64,
            "the job running on its thread as it was sampled, as `switches.job` names it (a pool job's own number before it joined the graph); null for none, and in rows of a blaze before it",
        ),
        opt(
            "fiber",
            Type::U32,
            "the fiber that job ran on, as `switches.fiber` names it; null for none, and in rows of a blaze before it",
        ),
    ],
};

/// The raw addresses of a sample's stack, kept beside its names so a reader can name them
/// again (`images` says which file each is in) or join two runs by code, not by name.
const STACK_PCS: Column = opt(
    "stack_pcs",
    Type::Pcs,
    "the stack's addresses, outermost first, one a frame of the unwind (an inlined function's frames in `stack` share one): the last is where the thread was, each other a return address, so the call is the address before it; null in rows of a blaze before it",
);

/// Why the query engine ran what it ran: its run log (`blaze_query::db::take_log_in`), taken as
/// a build ends while the daemon keeps a history. One row a run that was not a first one (a
/// first run is `query_counts.ran_new` alone: a cold build's every run is one), and one row the
/// first untracked read of a run, sorted by query, key, view and cause. Verified rows are
/// `query_counts.verified`, counted, not listed.
pub const PROVENANCE: Table = Table {
    name: "provenance",
    doc: "one row a query run that was not a first run, with why it ran, and one an untracked read a run made",
    columns: &[
        col("build_id", Type::U64, "the build that ended when the log was taken"),
        col(
            "query_key",
            Type::Utf8,
            "the memo: its query and its key as the key's type shows it, `` `infer_query`(DefWithBodyId(FunctionId(..))) `` (its id where the key has no `Debug`), as `cause_key` names it where it is another run's cause",
        ),
        col("kind", Type::Dict, "the query, as `query_counts.query` names it"),
        col(
            "outcome",
            Type::Dict,
            "ran (a run of it), or untracked read (its run made a read the engine can't see)",
        ),
        opt(
            "cause",
            Type::Utf8,
            "for ran: changed (a read changed since it last ran), evicted (its value was let go and asked for again), untracked (its last run made an untracked read) or cycle (a stale provisional row, or a verify that re-entered a cycle)",
        ),
        col(
            "view",
            Type::U32,
            "the database view it ran in: each write of inputs opens one, so a build has one or more",
        ),
        opt(
            "cause_key",
            Type::Utf8,
            "for changed: the read that changed: an input's or tracked struct's field as `` `Input.field`(<row>) ``, its row by name where the front end names it (a file's text by its path, `` `FileText.text`(/src/lib.rs) ``) else its id, or a memo as `query_key` names it, whose own row says why it changed",
        ),
        opt("cause_kind", Type::Dict, "what `cause_key` is: input, tracked or function"),
        opt(
            "what",
            Type::Utf8,
            "for untracked read: what was read and why it can't be a tracked read (`blaze_query::db::Untracked::what`)",
        ),
    ],
};

/// The parts of the key of each thing a build computed again (a unit, a build script's run,
/// a codegen template), each by name with its hash: what `misses` explains against the last
/// build, kept whole so two builds' keys (two homes, two shells) can be diffed part by part.
pub const KEY_PARTS: Table = Table {
    name: "key_parts",
    doc: "one row a part of the key of a thing a build computed again (one row with no part for a key that exposes none)",
    columns: &[
        col("build_id", Type::U64, "the build that computed it"),
        col(
            "step",
            Type::Dict,
            "what was computed, as `misses.step`: `unit`, `build script run`, `codegen dev template`",
        ),
        col("item", Type::Utf8, "what of, as `misses.item`"),
        col("key", Type::Utf8, "the whole key, in hex, as the store files it"),
        opt(
            "part",
            Type::Utf8,
            "the part's name: `the toolchain`, `settings`, `env CC`; null for a key that exposes no parts",
        ),
        opt(
            "hash",
            Type::Utf8,
            "the part's hash, in hex; null when the key has no such part this time",
        ),
        opt(
            "names",
            Type::Utf8,
            "the item the part is of, when another's (a dependency's unit): what a chain of reasons follows",
        ),
        opt(
            "beyond",
            Type::Utf8,
            "what the value is filed under past its key that the parts do not explain (a unit's process variables, a build script's traced reads); null when nothing",
        ),
    ],
};

/// What changed for a build, and how blaze saw it.
pub const INPUTS: Table = Table {
    name: "inputs",
    doc: "one row an input that changed for a build",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("key", Type::Utf8, "the input: a path, a variable's name, a flag, a dependency"),
        col("kind", Type::Dict, "file, env, flag or dependency"),
        opt("detail", Type::Utf8, "what changed about it"),
        col("seen_by", Type::Dict, "watcher, rescan or request"),
    ],
};

/// One allocation from the global heap in every 4 MiB a thread allocates, with its stack
/// (`crate::heap`): what churns the heap, by call site.
pub const ALLOC_SAMPLES: Table = Table {
    name: "alloc_samples",
    doc: "one row a sampled allocation from the daemon's global heap: one every 4 MiB a thread allocates (`blaze_graph::arena::SAMPLE_EVERY`), the one that crossed it",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        col("t_ns", Type::I64, "when"),
        col(
            "bytes",
            Type::U64,
            "the bytes of allocation the sample stands for: the sampling interval, or as many of it as a larger block crossed; summed by `site`, the bytes allocated there, within a sample's",
        ),
        col("stack", Type::Stack, "the call stack, outermost first"),
        opt("span_id", Type::U64, "the innermost span its thread was inside"),
        STACK_PCS,
        opt(
            "tag",
            Type::Dict,
            "the blaze-mem tag of the task that made the block (`query`, `mir`, `codegen`, ...); null for one made outside any task",
        ),
        col("block_bytes", Type::U64, "the sampled block's own bytes"),
        opt(
            "site",
            Type::Utf8,
            "the call site: the innermost frame of `stack` outside the allocator and the collections that call it (`alloc::`, `core::`, `std::`, `hashbrown::`, ...); null when every frame is theirs",
        ),
    ],
};

/// One row a code image (the executable, a shared library) holding an address a flush's
/// samples wrote in `stack_pcs`, once a process: what names those addresses after the process
/// is gone.
pub const IMAGES: Table = Table {
    name: "images",
    doc: "one row a code image of a process that `stack_pcs` addresses are in: its file, identity and where it was loaded",
    columns: &[
        PROCESS,
        opt(
            "boot",
            Type::Dict,
            "the machine's boot it ran in (Linux's boot_id, macOS's boot session UUID): a process id is unique only within one; null for the executable of a fatal signal recovered after its process died, as its `panics` row",
        ),
        col("seen_ns", Type::I64, "when it was written"),
        col("path", Type::Utf8, "the file it was loaded from"),
        opt(
            "build_id",
            Type::Utf8,
            "its GNU build id (Linux) or Mach-O UUID (macOS), hex; null when it has none",
        ),
        opt(
            "blake3",
            Type::Utf8,
            "the BLAKE3 of its file, hex; null when it was not hashed, `unhashed` says why",
        ),
        opt(
            "unhashed",
            Type::Utf8,
            "why `blake3` is null: the file is gone, changed (its build id no longer matches) or not on disk (macOS's shared cache)",
        ),
        col("base", Type::U64, "the address its code starts at in the process"),
        col(
            "size",
            Type::U64,
            "the bytes of code from `base`: an address in [base, base + size) is in it",
        ),
        col(
            "slide",
            Type::U64,
            "how far it was loaded from the addresses its file gives: a file address is an address less this",
        ),
        col(
            "names",
            Type::Dict,
            "how blaze named its frames: dwarf (functions and inlined ones), symbols (no DWARF), exports (a shared library's exported symbols) or none",
        ),
    ],
};

/// The heap at the end of a build, by what holds it.
pub const HEAP_BY_OWNER: Table = Table {
    name: "heap_by_owner",
    doc: "one row an owner of live heap at the end of a build: a memo table per query kind, an arena",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("owner", Type::Dict, "what holds the bytes"),
        col("bytes", Type::U64, "live bytes"),
    ],
};

/// What a `tiles` request answers: the spans of a range, aggregated per pixel-wide bucket.
pub const TILES: Table = Table {
    name: "tiles",
    doc: "spans of a time range per (process, thread, depth, bucket): what the explorer draws zoomed out",
    columns: &[
        PROCESS,
        THREAD,
        col("depth", Type::U16, "the spans' depth"),
        col("t0_ns", Type::I64, "the bucket's start"),
        col("t1_ns", Type::I64, "the bucket's end"),
        col("count", Type::U32, "spans overlapping it"),
        col("busy_ns", Type::I64, "of the bucket, the time inside any of them"),
        col("name", Type::Dict, "the longest one's name"),
    ],
};

/// The tiles of the jobs' lanes: `tiles` with a lane per fiber job in place of a lane per
/// thread, so a job that waits shows its wait as a gap and a job that moved between threads
/// is one lane. A result, not stored: the explorer's `tiles` op with `"lane": "job"` makes it
/// from the `job_spans` view.
pub const JOB_TILES: Table = Table {
    name: "job_tiles",
    doc: "spans of a time range per (process, job, depth, bucket): the explorer's lane per fiber job",
    columns: &[
        PROCESS,
        col("job", Type::U64, "the job, as `switches` numbers it"),
        col("depth", Type::U16, "the spans' depth"),
        col("t0_ns", Type::I64, "the bucket's start"),
        col("t1_ns", Type::I64, "the bucket's end"),
        col("count", Type::U32, "spans overlapping it"),
        col("busy_ns", Type::I64, "of the bucket, the time inside any of them"),
        col("name", Type::Dict, "the longest one's name"),
    ],
};

/// The spans each thread is inside now, not yet rows of `spans`: what the live feed sends
/// beside the rows that closed, and what each flush of the history that wrote rows also writes,
/// so the history of a daemon that died says what it was inside at its last flush.
pub const OPEN: Table = Table {
    name: "open",
    doc: "one row a span a live thread was inside when the threads were read (`read_ns`), outermost first",
    columns: &[
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("depth", Type::U16, "its place on its thread's stack, 0 outermost"),
        col("name", Type::Dict, "the span's name"),
        col("target", Type::Dict, "its tracing target, as in `spans`"),
        col("start_ns", Type::I64, "when it began"),
        opt("crate", Type::Dict, "the crate it is of, as in `spans`"),
        opt("function", Type::Utf8, "the function or query key it is of, as in `spans`"),
        opt("unit", Type::Utf8, "the codegen unit it is of, as in `spans`"),
        opt("awaits", Type::Utf8, "what a wait waits for, as in `spans`"),
        opt("owner", Type::U64, "who runs what it waits for, as in `spans`"),
        col("read_ns", Type::I64, "when the threads were read: one value a reading"),
        opt(
            "task",
            Type::U64,
            "the nearest task among the span and its ancestors: a daemon `task` span's plane request << 32 | node, the id of the explorer's `tasks` row",
        ),
    ],
};

/// One row an error a build said, so "derive panics per build" is a query, not a search of a
/// log. Rows are written by `History::errored` (observe-history's `errors.rs`).
pub const ERRORS: Table = Table {
    name: "errors",
    doc: "one row an error a build said: a diagnostic of level error, or each function a refused-functions error names",
    columns: &[
        BUILD_ID,
        opt("t_ns", Type::I64, "when the daemon said it"),
        opt("crate", Type::Dict, "the package of the unit it is of"),
        opt("unit", Type::Utf8, "the unit as cargo names it: `serde (build script)`"),
        col(
            "kind",
            Type::Dict,
            "a stable code: rustc's (E0277) or a deny lint's name, else blaze's: derive-panic, attr-panic, macro-panic, expand-failed, link-native-missing, link-failed, template-refs, refused, daemon-panic, blaze-bug (a site of a bug the daemon went on past: `function` its value kind, `message` what and how often, `detail` the first report's thread and stack), uncoded",
        ),
        opt(
            "function",
            Type::Utf8,
            "the function or instance it is of: a refused instance, a panicking proc macro",
        ),
        col("message", Type::Utf8, "its first line"),
        opt("detail", Type::Utf8, "why: its first help or note, or a refused function's reason"),
        opt("file", Type::Utf8, "the file of its primary span"),
        opt("line", Type::U32, "its primary span's line, from 1"),
    ],
};

/// One row an image a build linked: patched in place or laid out whole (`crate::facts`).
pub const LINKS: Table = Table {
    name: "links",
    doc: "one row an image a build linked, and how",
    columns: &[
        BUILD_ID,
        col("image", Type::Utf8, "the image: the target as cargo names it, `blaze (bin)`"),
        opt("crate", Type::Dict, "the package it is of"),
        col(
            "outcome",
            Type::Dict,
            "patched (the kept image patched in place) or placed (laid out whole)",
        ),
        opt("dur_ns", Type::I64, "how long linking it took"),
    ],
};

/// One row an edge of the crate graph a build built (`crate::facts`).
pub const CRATE_DEPS: Table = Table {
    name: "crate_deps",
    doc: "one row a dependency of a crate a build built",
    columns: &[
        BUILD_ID,
        col("crate", Type::Dict, "the crate that depends"),
        col("dep", Type::Dict, "the crate it depends on"),
        col("kind", Type::Dict, "normal, build or dev: the dependency table it is from"),
    ],
};

/// One row a query function and a build: what the query engine counted of it, taken when the
/// build ended (`crate::facts`). The counts are the process's, so of two builds that overlap,
/// the first to end has both's.
pub const QUERY_COUNTS: Table = Table {
    name: "query_counts",
    doc: "one row a query function a build ran, verified or evicted: what the query engine counted",
    columns: &[
        col("build_id", Type::U64, "the build that ended when they were taken"),
        col(
            "query",
            Type::Dict,
            "the query function, as `spans.query_kind` names a query's span where they agree",
        ),
        col("executed", Type::U64, "its runs"),
        col("verified", Type::U64, "its rows proved unchanged without running"),
        col("reran", Type::U64, "of its runs, those in a view the row had already run in"),
        col(
            "evicted_asked",
            Type::U64,
            "of its runs, those for a value evicted earlier and asked for again: what eviction cost",
        ),
        col(
            "evicted",
            Type::U64,
            "its values a pass let go to keep its tag's values under their budget",
        ),
        col(
            "dropped",
            Type::U64,
            "its values an explicit `evict_values` or `evict_values_of` let go",
        ),
        col(
            "reads_recorded",
            Type::U64,
            "the reads its runs recorded, a row read again later in the same run included: over `reads_kept`, how often its runs re-read the same rows",
        ),
        col(
            "reads_kept",
            Type::U64,
            "the reads its runs' rows kept: each run's distinct reads, summed",
        ),
        col(
            "reads_most",
            Type::U64,
            "the most reads one of its runs held on its thread's query stack as it ended (the build's most, not a sum)",
        ),
        opt(
            "written",
            Type::U64,
            "of an input: the times a setter wrote one of its fields, an equal value too (each opens a view): what the build changed, behind every run after it; null in files of a blaze before it",
        ),
        opt(
            "ran_new",
            Type::U64,
            "of its runs, first runs (the row never ran in this database); null in files of a blaze before it",
        ),
        opt(
            "ran_changed",
            Type::U64,
            "of its runs, those because a read changed since the row last ran (`provenance` names the read); null before it",
        ),
        opt(
            "ran_untracked",
            Type::U64,
            "of its runs, those because the row's last run made an untracked read, so it runs in every view (`provenance` names the read); null before it",
        ),
        opt(
            "ran_cycle",
            Type::U64,
            "of its runs, those for a cycle: a stale provisional row, or a verify that re-entered a cycle; null before it",
        ),
    ],
};

/// One row a thing a build computed again instead of reusing, and why (`blaze why`): what
/// the daemon worked out from the misses the build gave back (`blaze_kernel::why`), sorted by
/// item.
pub const MISSES: Table = Table {
    name: "misses",
    doc: "one row a thing a build computed again instead of reusing, and why",
    columns: &[
        col("build_id", Type::U64, "the build that computed it again"),
        col(
            "step",
            Type::Dict,
            "what was computed: `codegen dev template`, `unit`, `build script run`",
        ),
        col(
            "item",
            Type::Utf8,
            "what of, by path: a function, a unit, a package's build script's run",
        ),
        col(
            "cause",
            Type::Dict,
            "first, parts, lost, beyond, unnamed, read, unrecorded, collected, unshaped, twice, always, not_explained or failed",
        ),
        opt(
            "what",
            Type::Utf8,
            "what changed or why, in words: the read, the parts of its key that changed, the reason",
        ),
        opt(
            "names",
            Type::Utf8,
            "the item the change is of, when another's: what a chain of reasons follows",
        ),
        opt(
            "was",
            Type::Utf8,
            "the hash of what changed when it was last computed, in hex; null for several parts",
        ),
        opt("now", Type::Utf8, "its hash now, in hex; null for several parts"),
        opt(
            "changed",
            Type::U32,
            "how many of its reads or key parts changed; for twice, how many keys it had",
        ),
        opt("of", Type::U32, "how many reads or key parts it has"),
        opt(
            "since",
            Type::U64,
            "the last build that computed it and ended well, which it is compared with",
        ),
    ],
};

const JOB: Column =
    opt("job", Type::U64, "the job, as `switches` numbers it; null for a thread lane");
const STATE: Column = col(
    "state",
    Type::Utf8,
    "running, parked, queued, parked_or_queued (no `ready` row says where the park ended) or unrecorded",
);

// The tables `crate::time` derives from `switches` and `spans`, written into the history by
// `blaze debug time` as `<build>-0.<table>.parquet`: the first files the cap deletes, as they
// are made again on request.

/// One row a stretch of a job's lifetime in one state: its lifetime end to end.
pub const JOB_INTERVALS: Table = Table {
    name: "job_intervals",
    doc: "one row a stretch of a job's (or a thread lane's) lifetime in one state, its intervals end to end",
    columns: &[
        BUILD_ID,
        PROCESS,
        JOB,
        opt(
            "lane_thread",
            Type::U32,
            "for a thread lane, a thread no switch puts a job on: its thread",
        ),
        opt("fiber", Type::U32, "the fiber the job ran on"),
        STATE,
        col("from_ns", Type::I64, "when it began"),
        col("to_ns", Type::I64, "when it ended"),
        col("dur_ns", Type::I64, "to_ns - from_ns"),
        opt("core", Type::U32, "for running: the core it ran on"),
        opt("thread", Type::U32, "for running: the thread it ran on"),
        opt(
            "run_end",
            Type::Utf8,
            "for running, what ended it: switch (its own next switch), record_end (the record's end, the job still running) or spans (a thread lane's last span)",
        ),
        opt(
            "wait_kind",
            Type::Utf8,
            "for a wait: what it waited for, as `switches` says; null for a thread lane's wait span",
        ),
        opt("wait_value", Type::U64, "for a wait: its value, as `switches` says"),
        opt("awaits", Type::Utf8, "for a wait: its wait span's `awaits`"),
        opt("waker_job", Type::U64, "for a wait: the job that woke it, when the record names it"),
        opt(
            "waker",
            Type::Utf8,
            "for a wait: how its waker is known: ready (its ready row), awaited (the job it waited on), owner (the key's runner), no_job (woken from no fiber), no_lane (a job with no switch rows) or unknown (no row names it)",
        ),
        opt("span_id", Type::U64, "the innermost span of its lane open just before it ended"),
        opt(
            "unrecorded",
            Type::Utf8,
            "for unrecorded, the row the record lacks: cut (another job ran on its thread), no_end (no ended row) or between_spans (a thread lane between its spans)",
        ),
    ],
};

/// One row a span with its time by its job's states.
pub const SPAN_TIME: Table = Table {
    name: "span_time",
    doc: "one row a span, its open time split by its job's states; each row's states add up to its dur_ns",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("span_id", Type::U64, "as in `spans`"),
        JOB,
        THREAD,
        col("name", Type::Utf8, "as in `spans`"),
        opt("crate", Type::Utf8, "as in `spans`"),
        opt("function", Type::Utf8, "as in `spans`"),
        opt("unit", Type::Utf8, "as in `spans`"),
        col("start_ns", Type::I64, "as in `spans`"),
        col("dur_ns", Type::I64, "as in `spans`"),
        col("running_ns", Type::I64, "of it, its job ran on a core"),
        col("parked_ns", Type::I64, "of it, its job was parked before a `ready` row"),
        col("queued_ns", Type::I64, "of it, its job was runnable and waiting for a core"),
        col(
            "parked_or_queued_ns",
            Type::I64,
            "of it, its job waited with no `ready` row saying how long it was queued",
        ),
        col("unrecorded_ns", Type::I64, "of it, the record says nothing of its job"),
        col("uncovered_ns", Type::I64, "of its running time, the part under no child span"),
        col(
            "self_waiting_ns",
            Type::I64,
            "of its waiting, the part under no child span: for a span not `wait`, a wait with no name",
        ),
    ],
};

/// One row a link of a build's critical path.
pub const PATH_LINKS: Table = Table {
    name: "path_links",
    doc: "one row a link of a build's critical path, first link first, end to end",
    columns: &[
        BUILD_ID,
        col("step", Type::U64, "its place on the path, 0 first"),
        col(
            "kind",
            Type::Utf8,
            "ran, queued, waited (no waker to cross to), wake (the waker's last run to the resume) or unrecorded",
        ),
        PROCESS,
        JOB,
        col("lane", Type::Utf8, "the job or thread lane it is on"),
        opt("core", Type::U32, "for ran: the core"),
        col("from_ns", Type::I64, "when it began"),
        col("to_ns", Type::I64, "when it ended"),
        col("dur_ns", Type::I64, "to_ns - from_ns"),
        col(
            "item",
            Type::Utf8,
            "the span it ran inside and its outermost one, or what it waited for",
        ),
        opt(
            "woke_job",
            Type::U64,
            "the job whose wait this link's end woke: the path crossed from it",
        ),
        opt("woke", Type::Utf8, "what that job waited for"),
    ],
};

/// One row a core and a second of a build.
pub const CORE_BUSY: Table = Table {
    name: "core_busy",
    doc: "one row a core and a second of a build: how long jobs ran on it",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("core", Type::U32, "the driver's core"),
        col("t0_ns", Type::I64, "the second's start"),
        col("t1_ns", Type::I64, "its end (the build's, for the last)"),
        col("running_ns", Type::I64, "how long jobs ran on the core in it"),
    ],
};

/// One row a profiled run of a program blaze built: one or more repetitions of it, recorded by
/// `blaze perf record` (docs/v2/plan/design/perf-history.md). Its files are
/// `perf-<run>.perf_runs.parquet`, which the history's cap leaves alone.
pub const PERF_RUNS: Table = Table {
    name: "perf_runs",
    doc: "one row a profiled run of a program blaze built (blaze perf record)",
    columns: &[
        col("run", Type::Utf8, "16 hex digits: a hash of its repetitions' profiles"),
        col("format", Type::U32, "the perf rows' format: a reader refuses one it does not know"),
        col(
            "target",
            Type::Utf8,
            "the program, as its profile names it (its executable's file name)",
        ),
        col("started_unix_ns", Type::I64, "when its first repetition began, on the wall clock"),
        col("reps", Type::U32, "how many repetitions"),
        opt(
            "tree",
            Type::Utf8,
            "the digest of the workspace's source tree, as `builds.tree`; null when not known (`gaps` says why)",
        ),
        col("blaze", Type::Utf8, "the BLAKE3 hash of the blaze that recorded it"),
        opt(
            "binary",
            Type::Utf8,
            "the BLAKE3 hash of the executable that ran; null when it could not be read",
        ),
        opt("build_id", Type::Utf8, "the executable's build id, as its profile's frames name it"),
        col("host", Type::Utf8, "the machine it ran on"),
        col("arch", Type::Utf8, "its architecture"),
        col("os", Type::Utf8, "its operating system"),
        opt(
            "cpu_period_ns",
            Type::U64,
            "the CPU time one sample stands for; null when the sampler was off",
        ),
        col("sampler", Type::Utf8, "how the sampler ran, as the profile says"),
        col("samples", Type::U64, "samples of a thread on a CPU, every repetition"),
        col("unmapped", Type::U64, "of those, samples whose running function no code map names"),
        col("wall_ns", Type::I64, "the repetitions' wall time, summed"),
        opt("maps", Type::Utf8, "the directory of the code maps its rows were named through"),
        opt("gaps", Type::Utf8, "each input the run lacks, in words, `; ` between them"),
    ],
};

/// One row a function and a repetition of a profiled run: its samples, named by path and by
/// code identity, never by address.
pub const PERF_FUNCTIONS: Table = Table {
    name: "perf_functions",
    doc: "one row a function and a repetition of a profiled run: samples by path and code identity",
    columns: &[
        col("run", Type::Utf8, "the run, as `perf_runs.run`"),
        col("rep", Type::U32, "the repetition, from 0"),
        col(
            "function",
            Type::Utf8,
            "the instance's demangled path; the frame's symbol when no code map names it",
        ),
        opt("module", Type::Utf8, "the image it is in"),
        opt(
            "identity",
            Type::Utf8,
            "what it is across builds (item, which instance, type arguments), hex: what runs are joined by",
        ),
        opt("code", Type::Utf8, "its own code: its code hash and its type arguments' facts, hex"),
        opt("key", Type::Utf8, "its code with the codegen settings, hex"),
        opt("deep", Type::Utf8, "its key and those of everything it refers to, transitively, hex"),
        col("self_samples", Type::U64, "samples with it running"),
        col("total_samples", Type::U64, "samples with it anywhere on the stack, once a sample"),
        opt("calls", Type::U64, "times it was called, counted; null when not counted"),
        opt("instructions", Type::U64, "instructions it retired, counted; null when not counted"),
        opt("gap", Type::Utf8, "for a row with no identity: why"),
    ],
};

/// One row a panic or a fatal signal of a blaze process, with the stack it was raised on
/// (`crate::panics`, `crate::crash`). A panic is recorded by the process's panic hook as it is
/// raised, into a fixed ring the history drains at each flush. A fatal signal (SIGSEGV, SIGBUS,
/// SIGILL, SIGFPE, SIGABRT) is recorded by a signal handler into `<history>/crash.<pid>`, a file
/// mapped into the process, which the next daemon to open the history turns into a row of kind
/// `signal` and deletes. One table for both: each is a thread that failed at a stack, and "what
/// failed in build N" is one query; the columns only one of them fills are null in the other.
pub const PANICS: Table = Table {
    name: "panics",
    doc: "one row a panic or a fatal signal of a blaze process, with its stack and the spans its thread was inside",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("t_ns", Type::I64, "when it was raised"),
        col(
            "kind",
            Type::Dict,
            "panic, or signal (a fatal signal, recovered from the process's crash file after it died)",
        ),
        opt(
            "job",
            Type::U64,
            "the job running on the thread, as `switches.job` names it; null on no job and for a signal",
        ),
        opt(
            "span_id",
            Type::U64,
            "the innermost span its thread was inside, as `spans.span_id`; null outside any",
        ),
        opt(
            "message",
            Type::Utf8,
            "a panic's text (cut at 1 KiB, ending in `...`), null for a payload that is no text; for a signal its name and what it means, then the thread's last panic when it had one (an abort after a panic)",
        ),
        opt(
            "location",
            Type::Utf8,
            "a panic's `file:line:column`; for a signal, the location of the thread's last panic, if any",
        ),
        col(
            "stack",
            Type::Stack,
            "the call stack, outermost first, each frame named by function (and those inlined there) as `samples.stack`, the panic machinery's own frames included",
        ),
        col(
            "stack_pcs",
            Type::Pcs,
            "the stack's addresses, outermost first, one a frame of the unwind: named again after the process is gone through the `images` row of the same `process` and `boot` that holds each",
        ),
        col(
            "caught",
            Type::U32,
            "1 when a job's boundary catches it (a plane task's or a pool job's on its fiber), so only that job failed and the daemon kept serving; 0 for a panic nothing catches (the thread ends) and for a signal",
        ),
        col(
            "open_spans",
            Type::Stack,
            "the names of the spans its thread was inside, outermost first (the first 16)",
        ),
        opt(
            "signal",
            Type::Dict,
            "for a signal: its name (SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT); null for a panic",
        ),
        opt(
            "fault_addr",
            Type::U64,
            "for a signal: the address it is about (`si_addr`: the faulting access for SIGSEGV and SIGBUS); null for a panic",
        ),
        opt(
            "boot",
            Type::Dict,
            "the machine's boot the process ran in, as `images.boot`: with `process` the key of the `images` rows naming `stack_pcs`; null for a signal (recovered after the process died, perhaps after a restart) and where it can't be read",
        ),
    ],
};

/// One row a child process blaze spawned and reaped: a build script, a native proc-macro host,
/// a linker, a test binary, a wrapper (`crate::facts::ProcessRow`). The times are on the
/// history's clock (`clock::mono_ns`); the costs are the child's own, from its `rusage`.
pub const PROCESSES: Table = Table {
    name: "processes",
    doc: "one row a child process blaze spawned, with what the OS says it cost",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("pid", Type::U32, "the child's process id"),
        col(
            "kind",
            Type::Dict,
            "what it was: build_script, test, run, proc_macro_host, linker, wrapper, or unlabelled for a spawn no caller named",
        ),
        opt("unit", Type::Utf8, "the unit or test binary it ran for, as the spawner names it"),
        col("program", Type::Utf8, "the program it ran, as given to spawn"),
        opt("parent_span", Type::U64, "the span it was spawned inside, as `spans.span_id`"),
        col("spawned_ns", Type::I64, "when blaze asked for it"),
        opt(
            "exec_ns",
            Type::I64,
            "when the spawn call returned, the child having exec'd (vfork and posix_spawn return after exec)",
        ),
        col("exited_ns", Type::I64, "when blaze reaped it"),
        opt(
            "sandbox_ns",
            Type::I64,
            "how long making its sandbox took, before the spawn; null for a child run with none",
        ),
        col("user_ns", Type::I64, "its user CPU"),
        col("system_ns", Type::I64, "its system CPU"),
        opt("peak_rss", Type::U64, "its peak resident set, in bytes"),
        opt("minor_faults", Type::U64, "page faults it served without IO"),
        opt("major_faults", Type::U64, "page faults that read from disk"),
        opt("voluntary_switches", Type::U64, "times it gave up the CPU to wait"),
        opt("involuntary_switches", Type::U64, "times the kernel took the CPU from it"),
        opt("code", Type::U32, "its exit code as an unsigned word; null when a signal ended it"),
        opt("signal", Type::U32, "the signal that ended it; null for an exit"),
    ],
};

/// One row a test the test queue finished (`crate::facts::TestRunRow`).
pub const TEST_RUNS: Table = Table {
    name: "test_runs",
    doc: "one row a test a build ran, and how it ended",
    columns: &[
        BUILD_ID,
        col("test", Type::Utf8, "the test's name, as libtest names it"),
        opt("crate", Type::Dict, "the package its binary is of"),
        col("binary", Type::Utf8, "the test binary that ran it"),
        col("outcome", Type::Dict, "passed, failed, ignored, timed_out or crashed"),
        col("dur_ns", Type::I64, "how long its last attempt ran"),
        col("retries", Type::U32, "attempts after the first"),
        col("ended_ns", Type::I64, "when the queue took its outcome, on the history's clock"),
    ],
};

/// Every table the history and the live feed hold.
pub const TABLES: [&Table; 29] = [
    &SPANS,
    &COUNTERS,
    &EVENTS,
    &BUILDS,
    &SAMPLES,
    &PROVENANCE,
    &INPUTS,
    &ALLOC_SAMPLES,
    &HEAP_BY_OWNER,
    &SWITCHES,
    &ERRORS,
    &LINKS,
    &CRATE_DEPS,
    &QUERY_COUNTS,
    &JOB_INTERVALS,
    &SPAN_TIME,
    &PATH_LINKS,
    &CORE_BUSY,
    &MISSES,
    &PERF_RUNS,
    &PERF_FUNCTIONS,
    &OPEN,
    &FOLDED,
    &PANICS,
    &EVENT_FIELDS,
    &PROCESSES,
    &TEST_RUNS,
    &IMAGES,
    &KEY_PARTS,
];

/// The tables that are results, made on request and not kept.
pub const RESULTS: [&Table; 2] = [&TILES, &JOB_TILES];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_names_each_column_once() {
        for table in TABLES.iter().chain(&RESULTS).copied() {
            for (i, it) in table.columns.iter().enumerate() {
                assert_eq!(
                    table.column(it.name),
                    Some(i),
                    "{}.{} is named twice",
                    table.name,
                    it.name
                );
                assert!(!it.doc.is_empty(), "{}.{} says what it is", table.name, it.name);
            }
        }
    }

    #[test]
    fn the_explorers_columns_are_there() {
        let spans = [
            "build_id", "process", "thread", "span_id", "parent", "depth", "name", "start_ns",
            "dur_ns",
        ];
        assert!(spans.iter().all(|it| SPANS.column(it).is_some()));
        let counters = ["build_id", "process", "name", "t_ns", "value"];
        assert!(counters.iter().all(|it| COUNTERS.column(it).is_some()));
    }
}
