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
/// 0 and its spans' self time stays right. At 4: `events.message` is the message alone (it was
/// the message, then each other field as `name=value`); an event's other fields are rows of
/// `event_fields`, joined on `process` and `events.event_id`. A file of version 3 or older has no
/// `event_id` and its fields are in its `message`.
pub const VERSION: u32 = 4;

/// A version before [`VERSION`] whose files a reader reads as [`VERSION`]'s, and what reading
/// them so takes.
#[derive(Debug)]
pub struct Upgrade {
    pub from: u32,
    /// What changes between `from` and the next version, and why reading as the next is right.
    pub how: &'static str,
}

/// Each older version a reader reads, oldest last, with no rewrite. 3 reads as 4: its `events`
/// have no `event_id`, which the views read through the table's schema file as null, and the
/// loaders take a null `event_id` as an event with no `event_fields` rows; its fields stay in its
/// `message` as `name=value`, as 3 wrote them. 2 reads as 4 as 3 does and also has no
/// `folded_ns`, read as null, which every reader takes as 0, right as no blaze before 3 folded.
/// 1 does not: its `spans.owner` and `switches.wait_value` name a job by its slot + 1, and its
/// generation, which 2's `generation << 32 | slot` needs, was never written.
pub const UPGRADES: &[Upgrade] = &[
    Upgrade {
        from: 3,
        how: "no `events.event_id` column: read through the schema file as null, an event with no `event_fields` rows; its fields stay in its `message`",
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
        return Err("it is of a newer schema than this blaze reads: read it with the blaze that wrote it");
    }
    Err("it is of a schema this blaze has no upgrade from: its rows mean something no reader can say (version 1 named jobs without their generation); delete it, or the history")
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
        "the CPU time its job had while it was open, less that of the spans under it: a wait's is about 0, as other jobs run while it parks; null when the binary counts no CPU (`flight::count_cpu`) or it closed under another job",
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
        opt("fiber", Type::U32, "the fiber a fiber stack counter is of; null for any other counter"),
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
        opt("since_ns", Type::I64, "for started and ran, when the job began on its fiber, before it joined the graph"),
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
        col("target", Type::Dict, "its tracing target: the module path it was written in, unless it names one"),
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
        col("ordinal", Type::U16, "the field's place among its event's fields, in the order the event recorded them, from 0"),
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
        opt("value_str", Type::Utf8, "for kind str: its text; null when the text was let go before a reader read it"),
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
        col("root", Type::U32, "1 when they were roots (depth 0: a worker's own work between jobs, which `uncovered_per_worker` counts as covered), else 0"),
        col("from_ns", Type::I64, "when the first began"),
        col("to_ns", Type::I64, "when the last closed"),
        col("spans", Type::U64, "how many were folded"),
        col("dur_ns", Type::I64, "their durations summed"),
        col("max_dur_ns", Type::I64, "the longest"),
        opt("self_cpu_ns", Type::I64, "their self CPU summed, as `spans.self_cpu_ns`; null when not counted"),
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
        col("build_id", Type::U64, "the build's number, unique in the history: a daemon numbers its builds past every build its files hold rows of"),
        opt("commit", Type::Utf8, "the workspace's commit, when it is a git checkout"),
        col("workspace", Type::Utf8, "the workspace's root"),
        col("command", Type::Utf8, "what was asked: build, test, run, check, with the arguments"),
        col("started_ns", Type::I64, "when the daemon took it"),
        opt("dur_ns", Type::I64, "how long it ran; null while it runs"),
        opt("started_unix_ns", Type::I64, "when it was taken, on the wall clock"),
        col("outcome", Type::Dict, "ok, failed, cancelled, stalled, running or crashed (its daemon died while it ran; written by the postmortem of the daemon's flight recorder)"),
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
            "`on` when its thread was on a CPU; `off` when blocked (sampled on macOS), so what it blocked in shows",
        ),
    ],
};

/// Each query of a build that was not a plain memo hit, and why.
pub const PROVENANCE: Table = Table {
    name: "provenance",
    doc: "one row a query of a build that was verified or rerun, with the cause",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("query_key", Type::Utf8, "the query and its key"),
        col("kind", Type::Dict, "the query's kind"),
        col("outcome", Type::Dict, "verified, reran, or reran-same-output (an early cutoff)"),
        opt("cause", Type::Utf8, "the input or query key whose change made it rerun"),
        col("dur_ns", Type::I64, "how long verifying or running it took"),
        opt("unit", Type::Utf8, "the codegen unit it was for"),
        opt("crate", Type::Dict, "the crate it was of"),
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

/// One allocation in about every 512 KiB, with its stack.
pub const ALLOC_SAMPLES: Table = Table {
    name: "alloc_samples",
    doc: "one row a sampled allocation: about one every 512 KiB allocated, on each thread",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        col("t_ns", Type::I64, "when"),
        col("bytes", Type::U64, "the bytes the sample stands for (the sampling interval)"),
        col("stack", Type::Stack, "the call stack, outermost first"),
        opt("span_id", Type::U64, "the innermost span its thread was inside"),
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
        opt("function", Type::Utf8, "the function or instance it is of: a refused instance, a panicking proc macro"),
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
        col("outcome", Type::Dict, "patched (the kept image patched in place) or placed (laid out whole)"),
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
        col("query", Type::Dict, "the query function, as `spans.query_kind` names a query's span where they agree"),
        col("executed", Type::U64, "its runs"),
        col("verified", Type::U64, "its rows proved unchanged without running"),
        col("reran", Type::U64, "of its runs, those in a view the row had already run in"),
        col("evicted_asked", Type::U64, "of its runs, those for a value evicted earlier and asked for again: what eviction cost"),
        col("evicted", Type::U64, "its values a pass let go to keep its tag's values under their budget"),
        col("dropped", Type::U64, "its values an explicit `evict_values` or `evict_values_of` let go"),
        col("reads_recorded", Type::U64, "the reads its runs recorded, a row read again later in the same run included: over `reads_kept`, how often its runs re-read the same rows"),
        col("reads_kept", Type::U64, "the reads its runs' rows kept: each run's distinct reads, summed"),
        col("reads_most", Type::U64, "the most reads one of its runs held on its thread's query stack as it ended (the build's most, not a sum)"),
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
        col("step", Type::Dict, "what was computed: `codegen dev template`, `unit`, `build script run`"),
        col("item", Type::Utf8, "what of, by path: a function, a unit, a package's build script's run"),
        col(
            "cause",
            Type::Dict,
            "first, parts, lost, beyond, unnamed, read, unrecorded, collected, unshaped, twice, always, not_explained or failed",
        ),
        opt("what", Type::Utf8, "what changed or why, in words: the read, the parts of its key that changed, the reason"),
        opt("names", Type::Utf8, "the item the change is of, when another's: what a chain of reasons follows"),
        opt("was", Type::Utf8, "the hash of what changed when it was last computed, in hex; null for several parts"),
        opt("now", Type::Utf8, "its hash now, in hex; null for several parts"),
        opt("changed", Type::U32, "how many of its reads or key parts changed; for twice, how many keys it had"),
        opt("of", Type::U32, "how many reads or key parts it has"),
        opt("since", Type::U64, "the last build that computed it and ended well, which it is compared with"),
    ],
};

const JOB: Column = opt("job", Type::U64, "the job, as `switches` numbers it; null for a thread lane");
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
        opt("lane_thread", Type::U32, "for a thread lane, a thread no switch puts a job on: its thread"),
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
        opt("wait_kind", Type::Utf8, "for a wait: what it waited for, as `switches` says; null for a thread lane's wait span"),
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
        col("parked_or_queued_ns", Type::I64, "of it, its job waited with no `ready` row saying how long it was queued"),
        col("unrecorded_ns", Type::I64, "of it, the record says nothing of its job"),
        col("uncovered_ns", Type::I64, "of its running time, the part under no child span"),
        col("self_waiting_ns", Type::I64, "of its waiting, the part under no child span: for a span not `wait`, a wait with no name"),
    ],
};

/// One row a link of a build's critical path.
pub const PATH_LINKS: Table = Table {
    name: "path_links",
    doc: "one row a link of a build's critical path, first link first, end to end",
    columns: &[
        BUILD_ID,
        col("step", Type::U64, "its place on the path, 0 first"),
        col("kind", Type::Utf8, "ran, queued, waited (no waker to cross to), wake (the waker's last run to the resume) or unrecorded"),
        PROCESS,
        JOB,
        col("lane", Type::Utf8, "the job or thread lane it is on"),
        opt("core", Type::U32, "for ran: the core"),
        col("from_ns", Type::I64, "when it began"),
        col("to_ns", Type::I64, "when it ended"),
        col("dur_ns", Type::I64, "to_ns - from_ns"),
        col("item", Type::Utf8, "the span it ran inside and its outermost one, or what it waited for"),
        opt("woke_job", Type::U64, "the job whose wait this link's end woke: the path crossed from it"),
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
/// `blaze perf record` (docs/v2/plan/lanes/perf-history.md). Its files are
/// `perf-<run>.perf_runs.parquet`, which the history's cap leaves alone.
pub const PERF_RUNS: Table = Table {
    name: "perf_runs",
    doc: "one row a profiled run of a program blaze built (blaze perf record)",
    columns: &[
        col("run", Type::Utf8, "16 hex digits: a hash of its repetitions' profiles"),
        col("format", Type::U32, "the perf rows' format: a reader refuses one it does not know"),
        col("target", Type::Utf8, "the program, as its profile names it (its executable's file name)"),
        col("started_unix_ns", Type::I64, "when its first repetition began, on the wall clock"),
        col("reps", Type::U32, "how many repetitions"),
        opt("tree", Type::Utf8, "the digest of the workspace's source tree, as `builds.tree`; null when not known (`gaps` says why)"),
        col("blaze", Type::Utf8, "the BLAKE3 hash of the blaze that recorded it"),
        opt("binary", Type::Utf8, "the BLAKE3 hash of the executable that ran; null when it could not be read"),
        opt("build_id", Type::Utf8, "the executable's build id, as its profile's frames name it"),
        col("host", Type::Utf8, "the machine it ran on"),
        col("arch", Type::Utf8, "its architecture"),
        col("os", Type::Utf8, "its operating system"),
        opt("cpu_period_ns", Type::U64, "the CPU time one sample stands for; null when the sampler was off"),
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
        col("function", Type::Utf8, "the instance's demangled path; the frame's symbol when no code map names it"),
        opt("module", Type::Utf8, "the image it is in"),
        opt("identity", Type::Utf8, "what it is across builds (item, which instance, type arguments), hex: what runs are joined by"),
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
        col("kind", Type::Dict, "panic, or signal (a fatal signal, recovered from the process's crash file after it died)"),
        opt("job", Type::U64, "the job running on the thread, as `switches.job` names it; null on no job and for a signal"),
        opt("span_id", Type::U64, "the innermost span its thread was inside, as `spans.span_id`; null outside any"),
        opt(
            "message",
            Type::Utf8,
            "a panic's text (cut at 1 KiB, ending in `...`), null for a payload that is no text; for a signal its name and what it means, then the thread's last panic when it had one (an abort after a panic)",
        ),
        opt("location", Type::Utf8, "a panic's `file:line:column`; for a signal, the location of the thread's last panic, if any"),
        col("stack", Type::Stack, "the call stack, outermost first, each frame named (`function+offset`), the panic machinery's own frames included"),
        col(
            "stack_pcs",
            Type::Pcs,
            "the same stack as raw return addresses, outermost first, for naming again with the image `image_hash` and `image_base` name",
        ),
        opt("image_hash", Type::Utf8, "the BLAKE3 hash of the executable the addresses are of, as `builds.blaze`; null when it could not be read"),
        opt("image_base", Type::U64, "where that executable was loaded in the process (its first byte); addresses outside it are a shared library's"),
        col(
            "caught",
            Type::U32,
            "1 when a job's boundary catches it (a plane task's or a pool job's on its fiber), so only that job failed and the daemon kept serving; 0 for a panic nothing catches (the thread ends) and for a signal",
        ),
        col("open_spans", Type::Stack, "the names of the spans its thread was inside, outermost first (the first 16)"),
        opt("signal", Type::Dict, "for a signal: its name (SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT); null for a panic"),
        opt("fault_addr", Type::U64, "for a signal: the address it is about (`si_addr`: the faulting access for SIGSEGV and SIGBUS); null for a panic"),
    ],
};

/// Every table the history and the live feed hold.
pub const TABLES: [&Table; 25] = [
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
