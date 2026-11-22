//! Flecs-style ECS World, Telemetry Ingestion, Flame Graph Tree, Advisor Analyzer, and Zero-Dep JSON Parser.
//!
//! Directly consumes `crate::telemetry::{Event, EventKind, CPU_UNKNOWN}` from
//! `crates/blaze-launch/src/telemetry.rs` with zero schema duplication.

use std::collections::{HashMap, HashSet};

use crate::ecs::{Column, Entity, EntityAlloc, Rel, RelGraph};
use crate::rust_ide::{CompilerItemRow, FileGraph, SymDef};
use crate::telemetry::{Event, EventKind, CPU_UNKNOWN};

pub const MAX_SAMPLES: usize = 16_384;
pub const MAX_SPAN_HISTORY: usize = 4_096;
pub const MAX_ALLOC_HISTORY: usize = 4_096;
pub const MAX_PCT_SAMPLES: usize = 256;
pub const MAX_IO_ROWS: usize = 300;
pub const MAX_VITALS_BUCKETS: usize = 120; // 120 x 500ms = 60s rolling window
pub const VITALS_BUCKET_MS: f64 = 500.0;

#[derive(Clone, Debug)]
pub struct PcSym {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub slot: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct AllocRec {
    pub ts_ms: f64,
    pub site: String,
    pub pcs: [u64; 3],
    pub bytes: u64,
    pub is_free: bool,
    pub thread: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct VitalsBucket {
    pub bucket_id: u64,
    pub ts_ms: f64,
    pub cpu_ns: u64,
    pub lock_ns: u64,
    pub io_ns: u64,
    pub alloc_bytes: u64,
    pub retained_bytes: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub span_count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarKind {
    Span = 0,
    Heap = 1,
    Io = 2,
    Lock = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct TimelineBar {
    pub start_ms: f64,
    pub end_ms: f64,
    pub kind: BarKind,
    pub thread: u32,
    pub slot_or_op: u32,
    pub depth: u8,
    pub cpu: u16,
    pub flags: u8,
    pub dur_ns: u64,
    pub bytes: u64,
    pub holder_thread: u32,
    pub holder_pc: u64,
    pub waiter_pc: u64,
    pub mutex_ptr: u64,
    pub arg: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SpanRec {
    pub ts_ms: f64,
    pub slot: u32,
    pub span_id: u64,
    pub parent_id: u64,
    pub dur_ns: u64,
    pub thread: u32,
}

/// ECS Component for a Function / Slot entity.
#[derive(Clone, Debug)]
pub struct FnComp {
    pub slot: u32,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub rebuilt: bool,
    /// 0=plain, 1=count, 2=hook, 3=span
    pub hook: u8,
    pub calls: u64,
    pub total_ns: u64,
    pub child_ns: u64,
    pub max_ns: u64,
    pub last_cpu: u16,
    pub last_flags: u8,
    pub lock_contended: bool,
    pub samples: Vec<u64>,
}

/// ECS Component for a Thread entity.
#[derive(Clone, Debug)]
pub struct ThreadComp {
    pub tid: u32,
    pub parent_tid: u32,
    pub spawn_frames: [u64; 3],
    pub last_cpu: u16,
    pub last_flags: u8,
    pub migrations: u64,
    pub birth_ms: f64,
}

/// ECS Component for a 3-frame Heap Allocation Site entity.
#[derive(Clone, Debug)]
pub struct HeapSiteComp {
    pub key: String,
    pub pcs: [u64; 3],
    pub display_stack: String,
    pub top_fn: String,
    pub file: String,
    pub line: u32,
    pub slot: Option<u32>,
    pub thread: u32,
    pub allocs: u64,
    pub frees: u64,
    pub total_bytes: u64,
    pub retained_bytes: u64,
    pub min_bytes: u64,
    pub max_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct IoRow {
    pub kind: &'static str,
    pub thread: u32,
    pub cpu: u16,
    pub flags: u8,
    pub op: u32,
    pub dur_ns: u64,
    pub holder_thread: u32,
    pub holder_pc: u64,
    pub arg: u64,
    pub res: u64,
    pub ts_ms: f64,
}

#[derive(Clone, Debug)]
pub struct BuildPhase {
    pub name: String,
    pub ns: u64,
}

#[derive(Clone, Debug)]
pub struct BuildInfo {
    pub id: String,
    pub package: String,
    pub command: String,
    pub status: String,
    pub errors: u32,
    pub warnings: u32,
    pub reached: u32,
    pub found: u32,
    pub made: u32,
    pub recompile_reason: String,
    pub phases: Vec<BuildPhase>,
}

#[derive(Clone, Debug)]
pub struct HostTopology {
    pub arch: String,
    pub os: String,
    pub brand: String,
    pub ncpu: u32,
    pub p_cores: u32,
    pub e_cores: u32,
    pub numa_nodes: u32,
    pub l1d_bytes: u64,
    pub l2_bytes: u64,
    pub mem_bytes: u64,
}

impl Default for HostTopology {
    fn default() -> Self {
        Self {
            arch: "aarch64".into(),
            os: "macos".into(),
            brand: "Detecting host CPU…".into(),
            ncpu: 10,
            p_cores: 6,
            e_cores: 4,
            numa_nodes: 1,
            l1d_bytes: 131_072,
            l2_bytes: 16_777_216,
            mem_bytes: 34_359_738_368,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SocketInfo {
    pub proto: String,
    pub local: String,
    pub remote: String,
    pub state: String,
    pub port: u16,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessResources {
    pub pid: u32,
    pub fd_count: u32,
    pub sockets: Vec<SocketInfo>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Insight {
    pub severity: u8, // 0=ok, 1=warn, 2=critical
    pub category: &'static str,
    pub title: String,
    pub detail: String,
    pub metric: String,
    pub target_tab: usize,
    /// Optional function slot to open directly in `Code` Inspector on click.
    pub target_slot: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct FlameNode {
    pub name: String,
    pub slot: i64, // -1 for root
    pub total_ns: u64,
    pub self_ns: u64,
    pub calls: u64,
    pub max_ns: u64,
    pub children: Vec<FlameNode>,
}

/// The single Flecs-style ECS World owning all entities, SoA columns, and relationships.
pub struct ProfilerWorld {
    pub alloc: EntityAlloc,
    pub fns: Column<FnComp>,
    pub threads: Column<ThreadComp>,
    pub heap_sites: Column<HeapSiteComp>,
    pub rels: RelGraph,

    // O(1) domain ID -> ECS Entity lookup tables
    pub slot_entity: HashMap<u32, Entity>,
    pub thread_entity: HashMap<u32, Entity>,
    pub heap_entity: HashMap<String, Entity>,

    // Active run metadata
    pub run_id: String,
    pub package: String,
    pub pid: u32,
    pub run_alive: bool,
    pub connected: bool,
    pub conn_detail: String,
    pub total_events: u64,

    // Span parent/child map & bounded histories
    pub span_parent: HashMap<u64, (u32, u64)>, // span_id -> (slot, parent_id)
    pub span_history: Vec<SpanRec>,
    pub bars: Vec<TimelineBar>,
    pub latest_ts_ms: f64,
    /// Fixed wall-clock ms of the first event in this run (anchors ruler ticks at t = 0.00 s).
    pub trace_origin_ms: Option<f64>,
    /// Anchor mapping monotonic `ev.ts_ns` (when < 1e18) to browser wall-clock ms.
    pub ts_offset_ms: Option<f64>,

    // Live heap pointers + snapshot diff + bounded time-windowed alloc history
    pub live_ptrs: HashMap<u64, (Entity, u64)>,
    pub alloc_history: Vec<AllocRec>,
    pub heap_baseline: Option<HashMap<String, u64>>,
    pub heap_baseline_label: String,

    // I/O & Locks
    pub io_rows: Vec<IoRow>,

    // Rolling 60s time-series vitals (500ms buckets for CPU, Memory, Latency P50/P95/P99, Locks/IO)
    pub vitals: Vec<VitalsBucket>,
    pub cur_bucket_durs: Vec<u64>,
    pub total_cpu_ns: u64,
    pub total_lock_ns: u64,
    pub total_io_ns: u64,

    // Per-CPU hardware stats (cumulative + live decaying recent window for Per-Core Heatmap!)
    pub cpu_events: [u64; 128],
    pub cpu_busy_us: [u64; 128],
    pub cpu_live_us: [f32; 128],
    pub total_migrations: u64,
    pub topology: HostTopology,
    pub topology_loaded: bool,

    // Incremental build cards & static source locations
    pub builds: Vec<BuildInfo>,
    pub slot_names: HashMap<u32, String>,
    pub slot_locs: HashMap<u32, (String, u32, bool)>,
    pub pc_symbols: HashMap<u64, PcSym>,

    // Cached Rust source files (full-file + parsed IDE symbols + cross-file defs)
    pub sources: HashMap<(String, u32), Vec<(u32, String)>>,
    pub pending_sources: HashSet<(String, u32)>,
    pub file_lines: HashMap<String, Vec<String>>,
    pub file_graphs: HashMap<String, FileGraph>,
    pub pending_files: HashSet<String>,
    pub workspace_defs: HashMap<String, Option<SymDef>>,
    pub pending_defs: HashSet<String>,

    // Live OS process resources (open listening ports, TCP/UDP sockets, open FDs)
    pub resources: ProcessResources,
}

impl Default for ProfilerWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfilerWorld {
    pub fn new() -> Self {
        Self {
            alloc: EntityAlloc::default(),
            fns: Column::new(),
            threads: Column::new(),
            heap_sites: Column::new(),
            rels: RelGraph::default(),
            slot_entity: HashMap::new(),
            thread_entity: HashMap::new(),
            heap_entity: HashMap::new(),
            run_id: String::new(),
            package: "waiting…".into(),
            pid: 0,
            run_alive: false,
            connected: false,
            conn_detail: "connecting…".into(),
            total_events: 0,
            span_parent: HashMap::new(),
            span_history: Vec::with_capacity(1024),
            bars: Vec::with_capacity(2048),
            latest_ts_ms: 0.0,
            trace_origin_ms: None,
            ts_offset_ms: None,
            live_ptrs: HashMap::new(),
            alloc_history: Vec::with_capacity(512),
            heap_baseline: None,
            heap_baseline_label: String::new(),
            io_rows: Vec::with_capacity(MAX_IO_ROWS),
            vitals: Vec::with_capacity(MAX_VITALS_BUCKETS),
            cur_bucket_durs: Vec::with_capacity(128),
            total_cpu_ns: 0,
            total_lock_ns: 0,
            total_io_ns: 0,
            cpu_events: [0; 128],
            cpu_busy_us: [0; 128],
            cpu_live_us: [0.0; 128],
            total_migrations: 0,
            topology: HostTopology::default(),
            topology_loaded: false,
            builds: Vec::new(),
            slot_names: HashMap::new(),
            slot_locs: HashMap::new(),
            pc_symbols: HashMap::new(),
            sources: HashMap::new(),
            pending_sources: HashSet::new(),
            file_lines: HashMap::new(),
            file_graphs: HashMap::new(),
            pending_files: HashSet::new(),
            workspace_defs: HashMap::new(),
            pending_defs: HashSet::new(),
            resources: ProcessResources::default(),
        }
    }

    pub fn clear_trace(&mut self) {
        self.fns.clear();
        self.threads.clear();
        self.heap_sites.clear();
        self.rels.clear();
        self.slot_entity.clear();
        self.thread_entity.clear();
        self.heap_entity.clear();
        self.alloc.reset();
        self.span_parent.clear();
        self.span_history.clear();
        self.bars.clear();
        self.trace_origin_ms = None;
        self.ts_offset_ms = None;
        self.live_ptrs.clear();
        self.alloc_history.clear();
        self.heap_baseline = None;
        self.heap_baseline_label.clear();
        self.io_rows.clear();
        self.vitals.clear();
        self.cur_bucket_durs.clear();
        self.total_cpu_ns = 0;
        self.total_lock_ns = 0;
        self.total_io_ns = 0;
        self.cpu_events = [0; 128];
        self.cpu_busy_us = [0; 128];
        self.cpu_live_us = [0.0; 128];
        self.total_migrations = 0;
        self.total_events = 0;
    }

    pub fn ensure_fn(&mut self, slot: u32) -> Entity {
        if let Some(&e) = self.slot_entity.get(&slot) {
            return e;
        }
        let e = self.alloc.spawn();
        self.slot_entity.insert(slot, e);
        let name = self
            .slot_names
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| format!("slot #{slot}"));
        let (file, line, rebuilt) = self
            .slot_locs
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| (String::new(), 0, false));
        self.fns.insert(
            e,
            FnComp {
                slot,
                name,
                file,
                line,
                rebuilt,
                hook: 3, // Span
                calls: 0,
                total_ns: 0,
                child_ns: 0,
                max_ns: 0,
                last_cpu: CPU_UNKNOWN,
                last_flags: 0,
                lock_contended: false,
                samples: Vec::with_capacity(64),
            },
        );
        e
    }

    pub fn ensure_thread(&mut self, tid: u32, now_ms: f64) -> Entity {
        if let Some(&e) = self.thread_entity.get(&tid) {
            return e;
        }
        let e = self.alloc.spawn();
        self.thread_entity.insert(tid, e);
        self.threads.insert(
            e,
            ThreadComp {
                tid,
                parent_tid: 0,
                spawn_frames: [0; 3],
                last_cpu: CPU_UNKNOWN,
                last_flags: 0,
                migrations: 0,
                birth_ms: now_ms,
            },
        );
        e
    }

    pub fn fn_name(&self, slot: u32) -> String {
        if let Some(&e) = self.slot_entity.get(&slot) {
            if let Some(f) = self.fns.get(e) {
                return f.name.clone();
            }
        }
        self.slot_names
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| format!("slot #{slot}"))
    }

    pub fn resolve_pc(&self, pc: u64) -> Option<&PcSym> {
        if pc == 0 {
            return None;
        }
        self.pc_symbols.get(&pc)
    }

    pub fn pc_label(&self, pc: u64) -> String {
        if let Some(sym) = self.resolve_pc(pc) {
            return sym.name.clone();
        }
        format!("0x{pc:x}")
    }

    /// Resolve a 3-frame PC stack into `(display_stack, top_fn, slot, file, line)`.
    /// Uses `pc_symbols` first; if no PCs are symbolized yet, correlates with recent active spans on `thread`.
    pub fn resolve_stack(&self, pcs: [u64; 3], thread: u32) -> (String, String, Option<u32>, String, u32) {
        let mut names: Vec<String> = Vec::with_capacity(3);
        let mut any_resolved = false;
        let mut top_fn = String::new();
        let mut slot: Option<u32> = None;
        let mut file = String::new();
        let mut line = 0u32;

        for &pc in &pcs {
            if pc == 0 {
                continue;
            }
            if let Some(sym) = self.resolve_pc(pc) {
                any_resolved = true;
                if top_fn.is_empty() {
                    top_fn = sym.name.clone();
                    slot = sym.slot;
                    file = sym.file.clone();
                    line = sym.line;
                    if file.is_empty() {
                        if let Some(sl) = slot {
                            if let Some((f, l, _)) = self.slot_locs.get(&sl) {
                                file = f.clone();
                                line = *l;
                            }
                        }
                    }
                }
                names.push(sym.name.clone());
            } else {
                names.push(format!("0x{pc:x}"));
            }
        }

        if any_resolved {
            return (names.join(" → "), top_fn, slot, file, line);
        }

        // Fallback: correlate with the thread's most recent active span call chain
        if let Some(last_sp) = self.span_history.iter().rev().find(|s| s.thread == thread) {
            let mut chain: Vec<String> = Vec::with_capacity(3);
            let top_slot = last_sp.slot;
            let top_name = self.fn_name(top_slot);
            chain.push(top_name.clone());
            if let Some(&fe) = self.slot_entity.get(&top_slot) {
                if let Some(f) = self.fns.get(fe) {
                    file = f.file.clone();
                    line = f.line;
                }
            }
            if file.is_empty() {
                if let Some((f, l, _)) = self.slot_locs.get(&top_slot) {
                    file = f.clone();
                    line = *l;
                }
            }
            if last_sp.parent_id != 0 {
                if let Some(&(pslot, _)) = self.span_parent.get(&last_sp.parent_id) {
                    chain.push(self.fn_name(pslot));
                }
            }
            return (chain.join(" → "), top_name, Some(top_slot), file, line);
        }

        let fallback = if names.is_empty() {
            "unknown".into()
        } else {
            names.join(" → ")
        };
        let top = names.first().cloned().unwrap_or_else(|| "unknown".into());
        (fallback, top, None, String::new(), 0)
    }

    fn apply_slot_metadata(&mut self) {
        let names = self.slot_names.clone();
        let locs = self.slot_locs.clone();
        for (_, f) in self.fns.iter_mut() {
            if let Some(n) = names.get(&f.slot) {
                f.name = n.clone();
            }
            if let Some((file, line, rebuilt)) = locs.get(&f.slot) {
                f.file = file.clone();
                f.line = *line;
                f.rebuilt = *rebuilt;
            }
        }
        // Re-symbolize existing heap sites whenever new slot/PC metadata arrives
        let keys: Vec<(Entity, [u64; 3], u32)> = self
            .heap_sites
            .iter()
            .map(|(e, s)| (e, s.pcs, s.thread))
            .collect();
        for (e, pcs, thr) in keys {
            let (disp, top_fn, slot, file, line) = self.resolve_stack(pcs, thr);
            if let Some(hs) = self.heap_sites.get_mut(e) {
                hs.display_stack = disp;
                hs.top_fn = top_fn;
                hs.slot = slot;
                hs.file = file;
                hs.line = line;
            }
        }
    }

    /// ECS Phase `OnIngest`: zero-copy ingestion of packed 64-byte `Event` frames from binary WebSocket.
    pub fn ingest_binary(&mut self, raw: &[u8], now_ms: f64) -> u32 {
        let count = raw.len() / core::mem::size_of::<Event>();
        for i in 0..count {
            let off = i * 64;
            let ev = unsafe {
                let mut uninit = core::mem::MaybeUninit::<Event>::uninit();
                core::ptr::copy_nonoverlapping(
                    raw.as_ptr().add(off),
                    uninit.as_mut_ptr() as *mut u8,
                    64,
                );
                uninit.assume_init()
            };
            self.ingest_event(&ev, now_ms);
        }
        count as u32
    }

    fn touch_vitals(&mut self, ts_ms: f64) -> usize {
        let bid = (ts_ms.max(0.0) / VITALS_BUCKET_MS).floor() as u64;
        if let Some(last) = self.vitals.last() {
            if last.bucket_id == bid {
                return self.vitals.len() - 1;
            }
            if bid < last.bucket_id {
                // Out-of-order older event; attribute to latest bucket cleanly
                return self.vitals.len() - 1;
            }
        }
        self.cur_bucket_durs.clear();
        // Smooth exponential decay on per-core live load each 500ms window
        for v in &mut self.cpu_live_us {
            *v *= 0.62;
            if *v < 10.0 {
                *v = 0.0;
            }
        }
        let ret = self.total_retained_bytes();
        self.vitals.push(VitalsBucket {
            bucket_id: bid,
            ts_ms: (bid as f64 + 0.5) * VITALS_BUCKET_MS,
            retained_bytes: ret,
            ..VitalsBucket::default()
        });
        if self.vitals.len() > MAX_VITALS_BUCKETS {
            self.vitals.remove(0);
        }
        self.vitals.len() - 1
    }

    pub fn ingest_event(&mut self, ev: &Event, now_ms: f64) {
        self.total_events = self.total_events.saturating_add(1);
        // Derive true event timestamp in ms from `ev.ts_ns` so batched WebSocket deliveries
        // NEVER clump or overlap distinct spans onto the same arrival `Date.now()`.
        let ev_ms = if ev.ts_ns > 1_000_000_000_000_000_000 {
            // Unix epoch nanoseconds (e.g. producer or real clock_gettime)
            ev.ts_ns as f64 / 1_000_000.0
        } else if ev.ts_ns > 0 {
            let raw_ms = ev.ts_ns as f64 / 1_000_000.0;
            let off = *self.ts_offset_ms.get_or_insert(now_ms - raw_ms);
            raw_ms + off
        } else {
            now_ms
        };
        let now_ms = ev_ms;
        self.trace_origin_ms.get_or_insert(now_ms);
        if now_ms > self.latest_ts_ms {
            self.latest_ts_ms = now_ms;
        }

        // Update thread entity + CPU core + migration tracking
        let te = self.ensure_thread(ev.thread, now_ms);
        if ev.cpu != CPU_UNKNOWN && (ev.cpu as usize) < 128 {
            let cidx = ev.cpu as usize;
            self.cpu_events[cidx] = self.cpu_events[cidx].saturating_add(1);
            if let Some(tc) = self.threads.get_mut(te) {
                if tc.last_cpu != CPU_UNKNOWN && tc.last_cpu != ev.cpu {
                    tc.migrations = tc.migrations.saturating_add(1);
                    self.total_migrations = self.total_migrations.saturating_add(1);
                }
                tc.last_cpu = ev.cpu;
                tc.last_flags = ev.flags;
            }
        }

        match ev.kind {
            x if x == EventKind::SpanExit as u8 || x == EventKind::SpanUnwound as u8 => {
                let dur_ns = ev.a;
                let span_id = ev.b;
                let parent_id = ev.c;
                let slot = ev.slot_or_op;

                if ev.cpu != CPU_UNKNOWN && (ev.cpu as usize) < 128 {
                    let us = dur_ns / 1_000;
                    self.cpu_busy_us[ev.cpu as usize] =
                        self.cpu_busy_us[ev.cpu as usize].saturating_add(us);
                    // Only accumulate root spans into live core meter so parent+child aren't double-counted
                    if parent_id == 0 {
                        self.cpu_live_us[ev.cpu as usize] =
                            (self.cpu_live_us[ev.cpu as usize] + us as f32).min(1_200_000.0);
                    }
                }

                let fe = self.ensure_fn(slot);
                if let Some(fc) = self.fns.get_mut(fe) {
                    fc.calls = fc.calls.saturating_add(1);
                    fc.total_ns = fc.total_ns.saturating_add(dur_ns);
                    if dur_ns > fc.max_ns {
                        fc.max_ns = dur_ns;
                    }
                    if ev.cpu != CPU_UNKNOWN {
                        fc.last_cpu = ev.cpu;
                        fc.last_flags = ev.flags;
                    }
                    if fc.samples.len() < MAX_PCT_SAMPLES {
                        fc.samples.push(dur_ns);
                    } else {
                        let idx = (fc.calls as usize) % MAX_PCT_SAMPLES;
                        fc.samples[idx] = dur_ns;
                    }
                }

                if span_id != 0 {
                    if self.span_parent.len() > 6000 {
                        self.span_parent.clear();
                    }
                    self.span_parent.insert(span_id, (slot, parent_id));
                }
                if parent_id != 0 {
                    if let Some(&(parent_slot, _)) = self.span_parent.get(&parent_id) {
                        let parent_e = self.ensure_fn(parent_slot);
                        if let Some(pf) = self.fns.get_mut(parent_e) {
                            pf.child_ns = pf.child_ns.saturating_add(dur_ns);
                        }
                        // Flecs-style first-class call-graph relation: parent -(Calls)-> child!
                        self.rels.add_edge(parent_e, Rel::Calls, fe, 1, dur_ns, now_ms);
                    }
                }

                self.span_history.push(SpanRec {
                    ts_ms: now_ms,
                    slot,
                    span_id,
                    parent_id,
                    dur_ns,
                    thread: ev.thread,
                });
                if self.span_history.len() > MAX_SPAN_HISTORY {
                    self.span_history.drain(0..256);
                }

                let dur_ms = (dur_ns as f64 / 1_000_000.0).max(0.25);
                let depth = if parent_id != 0 { 1 } else { 0 };
                self.total_cpu_ns = self.total_cpu_ns.saturating_add(dur_ns);
                let vi = self.touch_vitals(now_ms);
                // Track root-span (entry-point) latency so P50/P95/P99 measures actual request/task duration,
                // not a noisy mix of parent + tiny child spans!
                if parent_id == 0 && self.cur_bucket_durs.len() < 256 {
                    self.cur_bucket_durs.push(dur_ns);
                }
                let p50 = percentile(&self.cur_bucket_durs, 0.50);
                let p95 = percentile(&self.cur_bucket_durs, 0.95);
                let p99 = percentile(&self.cur_bucket_durs, 0.99);
                let b = &mut self.vitals[vi];
                // Only add root spans to CPU time so parent+child aren't double-counted!
                if parent_id == 0 {
                    b.cpu_ns = b.cpu_ns.saturating_add(dur_ns);
                }
                b.span_count = b.span_count.saturating_add(1);
                if p50 > 0 {
                    b.p50_ns = p50;
                    b.p95_ns = p95;
                    b.p99_ns = p99;
                }
                self.push_bar(TimelineBar {
                    start_ms: now_ms - dur_ms,
                    end_ms: now_ms,
                    kind: BarKind::Span,
                    thread: ev.thread,
                    slot_or_op: slot,
                    depth,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    dur_ns,
                    bytes: 0,
                    holder_thread: 0,
                    holder_pc: 0,
                    waiter_pc: 0,
                    mutex_ptr: 0,
                    arg: 0,
                });
            }
            x if x == EventKind::HookHit as u8 => {
                let fe = self.ensure_fn(ev.slot_or_op);
                if let Some(fc) = self.fns.get_mut(fe) {
                    fc.calls = fc.calls.saturating_add(1);
                }
            }
            x if x == EventKind::Alloc as u8 => {
                let ptr = ev.a;
                let bytes = ev.b;
                let pcs = [ev.c, ev.d, ev.e];
                let key = format!("0x{:x} → 0x{:x} → 0x{:x}", ev.c, ev.d, ev.e);
                let he = if let Some(&e) = self.heap_entity.get(&key) {
                    e
                } else {
                    let (display_stack, top_fn, slot, file, line) = self.resolve_stack(pcs, ev.thread);
                    let e = self.alloc.spawn();
                    self.heap_entity.insert(key.clone(), e);
                    self.heap_sites.insert(
                        e,
                        HeapSiteComp {
                            key: key.clone(),
                            pcs,
                            display_stack,
                            top_fn,
                            file,
                            line,
                            slot,
                            thread: ev.thread,
                            allocs: 0,
                            frees: 0,
                            total_bytes: 0,
                            retained_bytes: 0,
                            min_bytes: u64::MAX,
                            max_bytes: 0,
                        },
                    );
                    e
                };
                if let Some(hs) = self.heap_sites.get_mut(he) {
                    hs.allocs = hs.allocs.saturating_add(1);
                    hs.total_bytes = hs.total_bytes.saturating_add(bytes);
                    hs.retained_bytes = hs.retained_bytes.saturating_add(bytes);
                    if bytes < hs.min_bytes {
                        hs.min_bytes = bytes;
                    }
                    if bytes > hs.max_bytes {
                        hs.max_bytes = bytes;
                    }
                    if hs.thread == 0 {
                        hs.thread = ev.thread;
                    }
                }
                if ptr != 0 {
                    self.live_ptrs.insert(ptr, (he, bytes));
                }
                self.alloc_history.push(AllocRec {
                    ts_ms: now_ms,
                    site: key,
                    pcs,
                    bytes,
                    is_free: false,
                    thread: ev.thread,
                });
                if self.alloc_history.len() > MAX_ALLOC_HISTORY {
                    self.alloc_history.drain(0..256);
                }
                let ret_now = self.total_retained_bytes();
                let vi = self.touch_vitals(now_ms);
                self.vitals[vi].alloc_bytes = self.vitals[vi].alloc_bytes.saturating_add(bytes);
                self.vitals[vi].retained_bytes = ret_now;
                self.push_bar(TimelineBar {
                    start_ms: now_ms - 16.0,
                    end_ms: now_ms,
                    kind: BarKind::Heap,
                    thread: ev.thread,
                    slot_or_op: ev.slot_or_op,
                    depth: 0,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    dur_ns: 0,
                    bytes,
                    holder_thread: 0,
                    holder_pc: ev.c,
                    waiter_pc: 0,
                    mutex_ptr: ptr,
                    arg: 0,
                });
            }
            x if x == EventKind::Free as u8 => {
                if let Some((he, old_bytes)) = self.live_ptrs.remove(&ev.a) {
                    let sub = if ev.b > 0 { ev.b } else { old_bytes };
                    let mut site_key = String::new();
                    let mut site_pcs = [0u64; 3];
                    if let Some(hs) = self.heap_sites.get_mut(he) {
                        hs.frees = hs.frees.saturating_add(1);
                        hs.retained_bytes = hs.retained_bytes.saturating_sub(sub);
                        site_key = hs.key.clone();
                        site_pcs = hs.pcs;
                    }
                    if !site_key.is_empty() {
                        self.alloc_history.push(AllocRec {
                            ts_ms: now_ms,
                            site: site_key,
                            pcs: site_pcs,
                            bytes: sub,
                            is_free: true,
                            thread: ev.thread,
                        });
                        if self.alloc_history.len() > MAX_ALLOC_HISTORY {
                            self.alloc_history.drain(0..256);
                        }
                    }
                }
            }
            x if x == EventKind::ThreadBirth as u8 => {
                let parent_tid = ev.a as u32;
                if let Some(tc) = self.threads.get_mut(te) {
                    tc.parent_tid = parent_tid;
                    tc.spawn_frames = [ev.b, ev.c, ev.d];
                    tc.birth_ms = now_ms;
                }
                if parent_tid != 0 {
                    let pe = self.ensure_thread(parent_tid, now_ms);
                    self.rels.add_edge(te, Rel::SpawnedBy, pe, 1, 0, now_ms);
                }
            }
            x if x == EventKind::SyncWait as u8 => {
                let dur_ns = ev.a;
                let mutex_ptr = ev.b;
                let waiter_pc = ev.c;
                let holder_thread = ev.d as u32;
                let holder_pc = ev.e;
                if holder_thread != 0 {
                    let he = self.ensure_thread(holder_thread, now_ms);
                    self.rels.add_edge(te, Rel::BlockedBy, he, 1, dur_ns, now_ms);
                }
                // Mark latest active function on this thread as lock-contended
                if let Some(last) = self.span_history.iter().rev().find(|s| s.thread == ev.thread) {
                    if let Some(&fe) = self.slot_entity.get(&last.slot) {
                        if let Some(fc) = self.fns.get_mut(fe) {
                            fc.lock_contended = true;
                        }
                    }
                }
                self.io_rows.push(IoRow {
                    kind: "sync_wait",
                    thread: ev.thread,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    op: ev.slot_or_op,
                    dur_ns,
                    holder_thread,
                    holder_pc,
                    arg: mutex_ptr,
                    res: waiter_pc,
                    ts_ms: now_ms,
                });
                if self.io_rows.len() > MAX_IO_ROWS {
                    self.io_rows.remove(0);
                }
                self.total_lock_ns = self.total_lock_ns.saturating_add(dur_ns);
                let vi = self.touch_vitals(now_ms);
                self.vitals[vi].lock_ns = self.vitals[vi].lock_ns.saturating_add(dur_ns);
                let dur_ms = (dur_ns as f64 / 1_000_000.0).max(12.0);
                self.push_bar(TimelineBar {
                    start_ms: now_ms - dur_ms,
                    end_ms: now_ms,
                    kind: BarKind::Lock,
                    thread: ev.thread,
                    slot_or_op: ev.slot_or_op,
                    depth: 0,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    dur_ns,
                    bytes: 0,
                    holder_thread,
                    holder_pc,
                    waiter_pc,
                    mutex_ptr,
                    arg: 0,
                });
            }
            x if x == EventKind::Io as u8 => {
                let dur_ns = ev.a;
                self.io_rows.push(IoRow {
                    kind: "io",
                    thread: ev.thread,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    op: ev.slot_or_op,
                    dur_ns,
                    holder_thread: 0,
                    holder_pc: 0,
                    arg: ev.b,
                    res: ev.c,
                    ts_ms: now_ms,
                });
                if self.io_rows.len() > MAX_IO_ROWS {
                    self.io_rows.remove(0);
                }
                self.total_io_ns = self.total_io_ns.saturating_add(dur_ns);
                let vi = self.touch_vitals(now_ms);
                self.vitals[vi].io_ns = self.vitals[vi].io_ns.saturating_add(dur_ns);
                let dur_ms = (dur_ns as f64 / 1_000_000.0).max(12.0);
                self.push_bar(TimelineBar {
                    start_ms: now_ms - dur_ms,
                    end_ms: now_ms,
                    kind: BarKind::Io,
                    thread: ev.thread,
                    slot_or_op: ev.slot_or_op,
                    depth: 0,
                    cpu: ev.cpu,
                    flags: ev.flags,
                    dur_ns,
                    bytes: 0,
                    holder_thread: 0,
                    holder_pc: 0,
                    waiter_pc: 0,
                    mutex_ptr: 0,
                    arg: ev.b,
                });
            }
            _ => {}
        }
    }

    fn push_bar(&mut self, bar: TimelineBar) {
        self.bars.push(bar);
        if self.bars.len() > MAX_SAMPLES {
            self.bars.drain(0..512);
        }
    }

    /// Snapshot current heap retained bytes for YourKit-style Δ diffing.
    pub fn take_heap_snapshot(&mut self, label: String) {
        let mut snap = HashMap::new();
        for (_, site) in self.heap_sites.iter() {
            snap.insert(site.key.clone(), site.retained_bytes);
        }
        self.heap_baseline = Some(snap);
        self.heap_baseline_label = label;
    }

    pub fn clear_heap_snapshot(&mut self) {
        self.heap_baseline = None;
        self.heap_baseline_label.clear();
    }

    pub fn total_retained_bytes(&self) -> u64 {
        self.heap_sites.iter().map(|(_, s)| s.retained_bytes).sum()
    }

    /// Returns heap allocation breakdown (either live totals or filtered to a dragged `[t0, t1]` window).
    pub fn heap_sites_for_window(&self, win: Option<(f64, f64)>) -> Vec<HeapSiteComp> {
        let Some((t0, t1)) = win else {
            let mut sites: Vec<_> = self.heap_sites.iter().map(|(_, s)| s.clone()).collect();
            sites.sort_by(|a, b| b.retained_bytes.cmp(&a.retained_bytes).then(b.total_bytes.cmp(&a.total_bytes)));
            return sites;
        };
        let mut by_site: HashMap<&str, HeapSiteComp> = HashMap::new();
        for rec in &self.alloc_history {
            if rec.ts_ms < t0 || rec.ts_ms > t1 {
                continue;
            }
            let entry = by_site.entry(rec.site.as_str()).or_insert_with(|| {
                let (display_stack, top_fn, slot, file, line) = self.resolve_stack(rec.pcs, rec.thread);
                HeapSiteComp {
                    key: rec.site.clone(),
                    pcs: rec.pcs,
                    display_stack,
                    top_fn,
                    file,
                    line,
                    slot,
                    thread: rec.thread,
                    allocs: 0,
                    frees: 0,
                    total_bytes: 0,
                    retained_bytes: 0,
                    min_bytes: u64::MAX,
                    max_bytes: 0,
                }
            });
            if rec.is_free {
                entry.frees = entry.frees.saturating_add(1);
                entry.retained_bytes = entry.retained_bytes.saturating_sub(rec.bytes);
            } else {
                entry.allocs = entry.allocs.saturating_add(1);
                entry.total_bytes = entry.total_bytes.saturating_add(rec.bytes);
                entry.retained_bytes = entry.retained_bytes.saturating_add(rec.bytes);
                if rec.bytes < entry.min_bytes {
                    entry.min_bytes = rec.bytes;
                }
                if rec.bytes > entry.max_bytes {
                    entry.max_bytes = rec.bytes;
                }
            }
        }
        let mut out: Vec<HeapSiteComp> = by_site.into_values().collect();
        out.sort_by(|a, b| b.retained_bytes.cmp(&a.retained_bytes).then(b.total_bytes.cmp(&a.total_bytes)));
        out
    }

    /// Computes live retained heap growth slope (bytes/sec) over recent vitals buckets.
    pub fn heap_slope_bps(&self) -> f64 {
        let n = self.vitals.len();
        if n < 4 {
            return 0.0;
        }
        let start_idx = n.saturating_sub(24); // last ~12s
        let first = self.vitals[start_idx];
        let last = self.vitals[n - 1];
        let dt_s = ((last.ts_ms - first.ts_ms) / 1000.0).max(0.5);
        (last.retained_bytes as f64 - first.retained_bytes as f64) / dt_s
    }

    /// Build a Top-Down Flame Graph tree from `span_history` (filtered by optional `[t0, t1]` range).
    pub fn build_flame_tree(&self, sel: Option<(f64, f64)>, inverted: bool) -> FlameNode {
        let mut root = FlameNode {
            name: format!("all ({})", self.package),
            slot: -1,
            total_ns: 0,
            self_ns: 0,
            calls: 0,
            max_ns: 0,
            children: Vec::new(),
        };

        let mut child_sum: HashMap<u64, u64> = HashMap::new();
        for r in &self.span_history {
            if let Some((t0, t1)) = sel {
                if r.ts_ms < t0 || r.ts_ms > t1 {
                    continue;
                }
            }
            if r.parent_id != 0 {
                *child_sum.entry(r.parent_id).or_default() += r.dur_ns;
            }
        }

        for r in &self.span_history {
            if let Some((t0, t1)) = sel {
                if r.ts_ms < t0 || r.ts_ms > t1 {
                    continue;
                }
            }
            let mut chain: Vec<(u32, String)> = Vec::with_capacity(6);
            chain.push((r.slot, self.fn_name(r.slot)));
            let mut pid = r.parent_id;
            for _ in 0..10 {
                if pid == 0 {
                    break;
                }
                if let Some(&(pslot, gpid)) = self.span_parent.get(&pid) {
                    chain.push((pslot, self.fn_name(pslot)));
                    pid = gpid;
                } else {
                    break;
                }
            }
            if !inverted {
                chain.reverse();
            }

            let csum = child_sum.get(&r.span_id).copied().unwrap_or(0);
            let self_ns = r.dur_ns.saturating_sub(csum);

            if inverted {
                if self_ns == 0 {
                    continue;
                }
                root.total_ns = root.total_ns.saturating_add(self_ns);
                root.calls = root.calls.saturating_add(1);
                let mut cur = &mut root;
                for (idx, (slot, name)) in chain.into_iter().enumerate() {
                    let pos = cur.children.iter().position(|c| c.name == name);
                    let child = if let Some(p) = pos {
                        &mut cur.children[p]
                    } else {
                        cur.children.push(FlameNode {
                            name,
                            slot: slot as i64,
                            total_ns: 0,
                            self_ns: 0,
                            calls: 0,
                            max_ns: 0,
                            children: Vec::new(),
                        });
                        cur.children.last_mut().unwrap()
                    };
                    child.total_ns = child.total_ns.saturating_add(self_ns);
                    if idx == 0 {
                        child.self_ns = child.self_ns.saturating_add(self_ns);
                    }
                    child.calls = child.calls.saturating_add(1);
                    if r.dur_ns > child.max_ns {
                        child.max_ns = r.dur_ns;
                    }
                    cur = child;
                }
            } else {
                if chain.len() == 1 {
                    root.total_ns = root.total_ns.saturating_add(r.dur_ns);
                    root.calls = root.calls.saturating_add(1);
                }
                let last_i = chain.len().saturating_sub(1);
                let mut cur = &mut root;
                for (i, (slot, name)) in chain.into_iter().enumerate() {
                    let pos = cur.children.iter().position(|c| c.name == name);
                    let child = if let Some(p) = pos {
                        &mut cur.children[p]
                    } else {
                        cur.children.push(FlameNode {
                            name,
                            slot: slot as i64,
                            total_ns: 0,
                            self_ns: 0,
                            calls: 0,
                            max_ns: 0,
                            children: Vec::new(),
                        });
                        cur.children.last_mut().unwrap()
                    };
                    if i == last_i {
                        child.total_ns = child.total_ns.saturating_add(r.dur_ns);
                        child.self_ns = child.self_ns.saturating_add(self_ns);
                        child.calls = child.calls.saturating_add(1);
                        if r.dur_ns > child.max_ns {
                            child.max_ns = r.dur_ns;
                        }
                    }
                    cur = child;
                }
            }
        }

        let child_tot: u64 = root.children.iter().map(|c| c.total_ns).sum();
        if child_tot > root.total_ns {
            root.total_ns = child_tot;
        }
        sort_flame(&mut root);
        root
    }

    /// ECS Phase `OnAnalyze`: 9-detector Automatic Anomaly Radar finding weird runtime behavior
    /// (heap leaks, E-core hot threads, core imbalance, tail jitter, lock convoys, hotspots, core hops, slow I/O, FD leaks).
    pub fn analyze_insights(&self) -> Vec<Insight> {
        let mut out = Vec::new();

        // 1. Build failures or cache misses
        for b in &self.builds {
            if b.errors > 0 {
                out.push(Insight {
                    severity: 2,
                    category: "BUILD",
                    title: format!("{} compile error", b.package),
                    detail: b.recompile_reason.clone(),
                    metric: format!("{} err", b.errors),
                    target_tab: 5,
                    target_slot: None,
                });
            } else if b.reached > 0 {
                let pct = (b.found as f64 / b.reached as f64) * 100.0;
                if pct < 82.0 {
                    out.push(Insight {
                        severity: 1,
                        category: "CACHE",
                        title: format!("{} cache miss", b.package),
                        detail: format!("{} rebuilt", b.made),
                        metric: format!("{:.0}% hit", pct),
                        target_tab: 5,
                        target_slot: None,
                    });
                }
            }
        }

        // 2. Heap Leak Detector (monotonic growth slope + top retaining call stack)
        let slope = self.heap_slope_bps();
        let ret = self.total_retained_bytes();
        if slope > 24_576.0 || ret >= 8 * 1024 * 1024 {
            let top_stack = self
                .heap_sites
                .iter()
                .max_by_key(|(_, s)| s.retained_bytes)
                .map(|(_, s)| {
                    format!(
                        "{} · {} ({} live)",
                        s.top_fn,
                        fmt_bytes(s.retained_bytes as i64),
                        self.live_ptrs.len()
                    )
                })
                .unwrap_or_else(|| format!("{} live", self.live_ptrs.len()));
            out.push(Insight {
                severity: if slope > 96_000.0 || ret >= 16 * 1024 * 1024 { 2 } else { 1 },
                category: "HEAP",
                title: if slope > 24_576.0 { "Heap leak slope".into() } else { "High retained heap".into() },
                detail: top_stack,
                metric: if slope > 1024.0 {
                    format!("+{}/s", fmt_bytes(slope as i64))
                } else {
                    fmt_bytes(ret as i64)
                },
                target_tab: 4,
                target_slot: None,
            });
        }

        // 3. Hot Work Stuck on an E-Core (Apple Silicon / hybrid big.LITTLE anomaly!)
        let p_cores = self.topology.p_cores as usize;
        if p_cores > 0 && self.topology.e_cores > 0 {
            let e_thread = self
                .threads
                .iter()
                .map(|(_, t)| t)
                .find(|t| t.last_cpu != CPU_UNKNOWN && ((t.last_flags & 0x10) != 0 || (t.last_cpu as usize) >= p_cores));
            if let Some(et) = e_thread {
                let busy_us = self.cpu_live_us.get(et.last_cpu as usize).copied().unwrap_or(0.0);
                if busy_us > 60_000.0 || self.cpu_busy_us.get(et.last_cpu as usize).copied().unwrap_or(0) > 250_000 {
                    out.push(Insight {
                        severity: 2,
                        category: "ECORE",
                        title: format!("t#{} running on E-core", et.tid),
                        detail: format!("Core {} while P-cores idle", et.last_cpu),
                        metric: format!("C{} · E", et.last_cpu),
                        target_tab: 2,
                        target_slot: None,
                    });
                }
            }
        }

        // 4. Mutex Contention / Convoy (waiter <- holder attribution)
        let lock_rows: Vec<&IoRow> = self.io_rows.iter().filter(|r| r.kind == "sync_wait").collect();
        if !lock_rows.is_empty() {
            let max_wait = lock_rows.iter().map(|r| r.dur_ns).max().unwrap_or(0);
            let last = lock_rows.last().unwrap();
            out.push(Insight {
                severity: if max_wait > 30_000_000 { 2 } else { 1 },
                category: "LOCK",
                title: if last.holder_thread > 0 {
                    format!("Mutex stall · t#{} ← t#{}", last.thread, last.holder_thread)
                } else {
                    format!("Mutex stall · t#{}", last.thread)
                },
                detail: format!("{}× waits · 0x{:x}", lock_rows.len(), last.arg),
                metric: fmt_ns(max_wait),
                target_tab: 7,
                target_slot: None,
            });
        }

        // 5. Hotspot Self-Time Monopoly & Tail-Latency Jitter (opens exact function in `Code`!)
        let total_self: u64 = self
            .fns
            .iter()
            .map(|(_, f)| f.total_ns.saturating_sub(f.child_ns))
            .sum();
        if total_self > 0 {
            let mut ranked: Vec<&FnComp> = self.fns.iter().map(|(_, f)| f).collect();
            ranked.sort_by(|a, b| {
                let sa = a.total_ns.saturating_sub(a.child_ns);
                let sb = b.total_ns.saturating_sub(b.child_ns);
                sb.cmp(&sa)
            });
            if let Some(f) = ranked.first() {
                let self_ns = f.total_ns.saturating_sub(f.child_ns);
                let pct = (self_ns as f64 / total_self as f64) * 100.0;
                if pct >= 22.0 {
                    out.push(Insight {
                        severity: if pct >= 40.0 { 2 } else { 1 },
                        category: "HOT",
                        title: f.name.clone(),
                        detail: format!("P95 {} · {}×", fmt_ns(percentile(&f.samples, 0.95)), f.calls),
                        metric: format!("{:.0}% self", pct),
                        target_tab: 3,
                        target_slot: Some(f.slot),
                    });
                }
            }
            // Tail jitter check (`P99 / P50 >= 1.75`)
            for f in ranked.iter() {
                if f.samples.len() >= 12 {
                    let p50 = percentile(&f.samples, 0.50).max(1);
                    let p99 = percentile(&f.samples, 0.99);
                    let ratio = p99 as f64 / p50 as f64;
                    if ratio >= 1.75 && p99 > 5_000_000 {
                        out.push(Insight {
                            severity: if ratio >= 3.0 { 2 } else { 1 },
                            category: "TAIL",
                            title: format!("{} tail jitter", f.name),
                            detail: format!("{} → {}", fmt_ns(p50), fmt_ns(p99)),
                            metric: format!("{ratio:.1}×"),
                            target_tab: 3,
                            target_slot: Some(f.slot),
                        });
                        break;
                    }
                }
            }
        }

        // 6. Cross-Core Thread Thrashing (names top thrashing thread)
        if let Some(thr) = self
            .threads
            .iter()
            .map(|(_, t)| t)
            .filter(|t| t.migrations >= 6)
            .max_by_key(|t| t.migrations)
        {
            out.push(Insight {
                severity: if self.total_migrations >= 64 { 2 } else { 1 },
                category: "CPU",
                title: format!("Core thrashing · t#{}", thr.tid),
                detail: format!("{} total hops across {} thr", self.total_migrations, self.threads.len()),
                metric: format!("{} hops", thr.migrations),
                target_tab: 2,
                target_slot: None,
            });
        }

        // 7. Slow Blocking I/O
        let max_io = self
            .io_rows
            .iter()
            .filter(|r| r.kind == "io")
            .max_by_key(|r| r.dur_ns);
        if let Some(io) = max_io {
            if io.dur_ns >= 30_000_000 {
                out.push(Insight {
                    severity: 1,
                    category: "IO",
                    title: format!("Blocking {} · t#{}", io_op_name(io.op), io.thread),
                    detail: format!("fd {} · {} B", io.arg, io.res),
                    metric: fmt_ns(io.dur_ns),
                    target_tab: 7,
                    target_slot: None,
                });
            }
        }

        // 8. High Open FD Count
        if self.resources.fd_count >= 256 {
            out.push(Insight {
                severity: if self.resources.fd_count >= 1024 { 2 } else { 1 },
                category: "FD",
                title: "High open FD count".into(),
                detail: format!("{} sockets open", self.resources.sockets.len()),
                metric: format!("{} FDs", self.resources.fd_count),
                target_tab: 7,
                target_slot: None,
            });
        }

        out.sort_by(|a, b| b.severity.cmp(&a.severity));
        out
    }

    pub fn ingest_resources_json(&mut self, json: &str) {
        let Some(v) = parse_json(json) else {
            return;
        };
        let mut res = ProcessResources {
            pid: v.get_u64("pid").unwrap_or(self.pid as u64) as u32,
            fd_count: v.get_u64("fd_count").unwrap_or(0) as u32,
            sockets: Vec::new(),
            files: Vec::new(),
        };
        if let Some(arr) = v.get_arr("sockets") {
            for s in arr.iter().take(48) {
                res.sockets.push(SocketInfo {
                    proto: s.get_str("proto").unwrap_or("TCP").to_string(),
                    local: s.get_str("local").unwrap_or("").to_string(),
                    remote: s.get_str("remote").unwrap_or("").to_string(),
                    state: s.get_str("state").unwrap_or("").to_string(),
                    port: s.get_u64("port").unwrap_or(0) as u16,
                });
            }
        }
        if let Some(arr) = v.get_arr("files") {
            for f in arr.iter().take(32) {
                if let Some(s) = f.as_str() {
                    res.files.push(s.to_string());
                }
            }
        }
        self.resources = res;
    }

    /// Ingest a JSON text control frame (`runs`, `builds`, `run_started`, `run_ended`) from WebSocket.
    pub fn ingest_json(&mut self, json: &str) {
        let Some(val) = parse_json(json) else {
            return;
        };
        let Some(ty) = val.get_str("type") else {
            return;
        };
        match ty {
            "runs" => {
                if let Some(arr) = val.get_arr("runs") {
                    if let Some(first) = arr.first() {
                        self.apply_run_obj(first);
                    }
                }
            }
            "run_started" => {
                if let Some(run) = val.get("run") {
                    self.apply_run_obj(run);
                }
            }
            "run_ended" => {
                if val.get_str("id") == Some(self.run_id.as_str()) {
                    self.run_alive = false;
                }
            }
            "builds" => {
                if let Some(arr) = val.get_arr("builds") {
                    self.builds.clear();
                    for b in arr.iter().rev() {
                        self.apply_build_obj(b);
                    }
                    self.apply_slot_metadata();
                }
            }
            _ => {}
        }
    }

    fn apply_run_obj(&mut self, obj: &JsonVal) {
        if let Some(id) = obj.get_str("id") {
            self.run_id = id.to_string();
        }
        if let Some(pkg) = obj.get_str("package") {
            self.package = pkg.to_string();
        }
        if let Some(pid) = obj.get_u64("pid") {
            self.pid = pid as u32;
        }
        if let Some(alive) = obj.get_bool("alive") {
            self.run_alive = alive;
        }
        if let Some(slots) = obj.get_obj("slots") {
            for (k, v) in slots {
                if let (Ok(id), Some(name)) = (k.parse::<u32>(), v.as_str()) {
                    self.slot_names.insert(id, name.to_string());
                }
            }
            self.apply_slot_metadata();
        }
        if let Some(pcs) = obj.get_obj("pc_symbols") {
            self.ingest_pc_symbols(pcs);
            self.apply_slot_metadata();
        }
    }

    fn ingest_pc_symbols(&mut self, map: &[(String, JsonVal)]) {
        for (k, v) in map {
            let Some(addr) = parse_pc_key(k) else {
                continue;
            };
            if let Some(name) = v.as_str() {
                self.pc_symbols.insert(
                    addr,
                    PcSym {
                        name: name.to_string(),
                        file: String::new(),
                        line: 0,
                        slot: None,
                    },
                );
            } else if let Some(name) = v.get_str("name") {
                self.pc_symbols.insert(
                    addr,
                    PcSym {
                        name: name.to_string(),
                        file: v.get_str("file").unwrap_or("").to_string(),
                        line: v.get_u64("line").unwrap_or(0) as u32,
                        slot: v.get_u64("slot").map(|x| x as u32),
                    },
                );
            }
        }
    }

    fn apply_build_obj(&mut self, b: &JsonVal) {
        let mut phases = Vec::new();
        if let Some(arr) = b.get_arr("phases") {
            for p in arr {
                phases.push(BuildPhase {
                    name: p.get_str("name").unwrap_or("").to_string(),
                    ns: p.get_u64("ns").unwrap_or(0),
                });
            }
        }
        if let Some(slots) = b.get_obj("slots") {
            for (k, v) in slots {
                if let (Ok(id), Some(name)) = (k.parse::<u32>(), v.as_str()) {
                    self.slot_names.insert(id, name.to_string());
                }
            }
        }
        if let Some(locs) = b.get_obj("locations") {
            for (k, v) in locs {
                if let Ok(id) = k.parse::<u32>() {
                    let name_opt = v.get_str("name").map(|s| s.to_string());
                    if let Some(ref name) = name_opt {
                        self.slot_names.insert(id, name.clone());
                    }
                    let file = v.get_str("file").unwrap_or("").to_string();
                    let line = v.get_u64("line").unwrap_or(0) as u32;
                    let rebuilt = v.get_bool("rebuilt").unwrap_or(false);
                    if !file.is_empty() {
                        self.slot_locs.insert(id, (file.clone(), line, rebuilt));
                    }
                    let addr = v
                        .get_str("pc")
                        .and_then(parse_pc_key)
                        .or_else(|| v.get_str("addr").and_then(parse_pc_key))
                        .or_else(|| v.get_u64("pc"))
                        .or_else(|| v.get_u64("addr"));
                    if let Some(pc) = addr {
                        let sym_name = name_opt
                            .or_else(|| self.slot_names.get(&id).cloned())
                            .unwrap_or_else(|| format!("slot #{id}"));
                        self.pc_symbols.insert(
                            pc,
                            PcSym {
                                name: sym_name,
                                file,
                                line,
                                slot: Some(id),
                            },
                        );
                    }
                }
            }
        }
        if let Some(pcs) = b.get_obj("pc_symbols") {
            self.ingest_pc_symbols(pcs);
        }
        self.builds.push(BuildInfo {
            id: b.get_str("id").unwrap_or("").to_string(),
            package: b.get_str("package").unwrap_or("").to_string(),
            command: b.get_str("command").unwrap_or("blaze run").to_string(),
            status: b.get_str("status").unwrap_or("ok").to_string(),
            errors: b.get_u64("errors").unwrap_or(0) as u32,
            warnings: b.get_u64("warnings").unwrap_or(0) as u32,
            reached: b.get_u64("reached").unwrap_or(0) as u32,
            found: b.get_u64("found").unwrap_or(0) as u32,
            made: b.get_u64("made").unwrap_or(0) as u32,
            recompile_reason: b.get_str("recompile_reason").unwrap_or("").to_string(),
            phases,
        });
    }

    pub fn ingest_topology_json(&mut self, json: &str) {
        let Some(v) = parse_json(json) else {
            return;
        };
        self.topology = HostTopology {
            arch: v.get_str("arch").unwrap_or("aarch64").to_string(),
            os: v.get_str("os").unwrap_or("macos").to_string(),
            brand: v.get_str("brand").unwrap_or("Apple Silicon").to_string(),
            ncpu: v.get_u64("ncpu").unwrap_or(10).clamp(1, 128) as u32,
            p_cores: v.get_u64("p_cores").unwrap_or(6) as u32,
            e_cores: v.get_u64("e_cores").unwrap_or(4) as u32,
            numa_nodes: v.get_u64("numa_nodes").unwrap_or(1).max(1) as u32,
            l1d_bytes: v.get_u64("l1d_bytes").unwrap_or(131_072),
            l2_bytes: v.get_u64("l2_bytes").unwrap_or(16_777_216),
            mem_bytes: v.get_u64("mem_bytes").unwrap_or(0),
        };
        self.topology_loaded = true;
    }

    pub fn ingest_source_json(&mut self, json: &str) {
        let Some(v) = parse_json(json) else {
            return;
        };
        let Some(file) = v.get_str("file") else {
            return;
        };
        let line = v.get_u64("line").unwrap_or(1) as u32;
        let is_full = v.get_bool("full").unwrap_or(false);
        let mut lines = Vec::new();
        if let Some(arr) = v.get_arr("lines") {
            for item in arr {
                let n = item.get_u64("n").unwrap_or(0) as u32;
                let text = item.get_str("text").unwrap_or("").to_string();
                lines.push((n, text));
            }
        }
        let fstr = file.to_string();
        self.pending_files.remove(&fstr);
        self.pending_sources.remove(&(fstr.clone(), line));
        if is_full || lines.first().map(|(n, _)| *n == 1).unwrap_or(false) {
            let full_vec: Vec<String> = lines.iter().map(|(_, t)| t.clone()).collect();
            if let Some(g) = v.get("graph") {
                let mut rows = Vec::new();
                if let Some(arr) = g.get_arr("items") {
                    for it in arr {
                        rows.push(CompilerItemRow {
                            kind: it.get_str("kind").unwrap_or("").to_string(),
                            name: it.get_str("name").unwrap_or("").to_string(),
                            owner: it.get_str("owner").unwrap_or("").to_string(),
                            sig: it.get_str("sig").unwrap_or("").to_string(),
                            ty: it.get_str("ty").unwrap_or("").to_string(),
                            doc: it.get_str("doc").unwrap_or("").to_string(),
                            line: it.get_u64("line").unwrap_or(1) as u32,
                            start_line: it.get_u64("start_line").unwrap_or(1) as u32,
                            end_line: it.get_u64("end_line").unwrap_or(1) as u32,
                        });
                    }
                }
                let uses: Vec<String> = g
                    .get_arr("uses")
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let submods: Vec<String> = g
                    .get_arr("submods")
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let graph = FileGraph::from_compiler(&fstr, &full_vec, rows, uses, submods);
                self.file_graphs.insert(fstr.clone(), graph);
            }
            self.file_lines.insert(fstr.clone(), full_vec);
        }
        self.sources.insert((fstr, line), lines);
    }

    pub fn ingest_def_json(&mut self, json: &str) {
        let Some(v) = parse_json(json) else {
            return;
        };
        let Some(key) = v.get_str("key").or_else(|| v.get_str("ident")) else {
            return;
        };
        let ident = v.get_str("ident").unwrap_or(key);
        self.pending_defs.remove(key);
        if !v.get_bool("found").unwrap_or(false) {
            self.workspace_defs.insert(key.to_string(), None);
            return;
        }
        let mut snippet = Vec::new();
        if let Some(arr) = v.get_arr("snippet") {
            for item in arr {
                let n = item.get_u64("n").unwrap_or(0) as u32;
                let text = item.get_str("text").unwrap_or("").to_string();
                snippet.push((n, text));
            }
        }
        let line = v.get_u64("line").unwrap_or(1) as u32;
        let def = SymDef {
            kind: v.get_str("kind").unwrap_or("sym").to_string(),
            name: v.get_str("display_name").unwrap_or(ident).to_string(),
            sig: v.get_str("sig").unwrap_or("").to_string(),
            doc: v.get_str("doc").unwrap_or("").to_string(),
            file: v.get_str("file").unwrap_or("").to_string(),
            line,
            start_line: v.get_u64("start_line").unwrap_or(line as u64) as u32,
            end_line: v.get_u64("end_line").unwrap_or(line as u64) as u32,
            owner: None,
            target_type: None,
            fields: Vec::new(),
            snippet,
        };
        self.workspace_defs.insert(key.to_string(), Some(def));
    }

    /// Returns a 5-line (or `count`-line) snippet centered at `line - 1` from full loaded files or snippet cache.
    pub fn snippet_for(&self, file: &str, line: u32, count: usize) -> Option<Vec<(u32, String)>> {
        if let Some(full) = self.file_lines.get(file) {
            if !full.is_empty() {
                let start = (line as usize).saturating_sub(2).min(full.len().saturating_sub(1));
                let end = (start + count).min(full.len());
                let mut out = Vec::with_capacity(end - start);
                for i in start..end {
                    out.push(((i + 1) as u32, full[i].clone()));
                }
                return Some(out);
            }
        }
        let v = self.sources.get(&(file.to_string(), line))?;
        let start_idx = v
            .iter()
            .position(|(n, _)| *n >= line.saturating_sub(1))
            .unwrap_or(0);
        Some(v.iter().skip(start_idx).take(count).cloned().collect())
    }
}

fn sort_flame(node: &mut FlameNode) {
    node.children.sort_by(|a, b| b.total_ns.cmp(&a.total_ns));
    for ch in &mut node.children {
        sort_flame(ch);
    }
}

pub fn percentile(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let idx = ((s.len() - 1) as f64 * p).floor() as usize;
    s[idx.min(s.len() - 1)]
}

pub fn io_op_name(op: u32) -> &'static str {
    match op {
        1 => "open",
        2 => "close",
        3 => "read",
        4 => "write",
        5 => "connect",
        6 => "getenv",
        7 => "mutex_lock",
        _ => "syscall",
    }
}

pub fn fmt_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.1} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    }
}

pub fn fmt_span(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms < 1000.0 {
        if ms < 10.0 {
            format!("{ms:.1} ms")
        } else {
            format!("{ms:.0} ms")
        }
    } else if ms < 10_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        format!("{:.0} s", ms / 1000.0)
    }
}

pub fn fmt_bytes(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let abs = n.unsigned_abs() as f64;
    if abs < 1024.0 {
        format!("{sign}{:.0} B", abs)
    } else if abs < 1024.0 * 1024.0 {
        format!("{sign}{:.1} KiB", abs / 1024.0)
    } else if abs < 1024.0 * 1024.0 * 1024.0 {
        format!("{sign}{:.2} MiB", abs / (1024.0 * 1024.0))
    } else {
        format!("{sign}{:.1} GiB", abs / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn short_path(path: &str) -> &str {
    if let Some(i) = path.find("crates/") {
        return &path[i..];
    }
    if let Some(i) = path.find("src/") {
        return &path[i..];
    }
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Zero-dependency recursive-descent JSON micro-parser (no serde needed!)
// ---------------------------------------------------------------------------

enum JsonVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

impl JsonVal {
    fn get(&self, key: &str) -> Option<&JsonVal> {
        if let JsonVal::Obj(entries) = self {
            entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }
    fn as_str(&self) -> Option<&str> {
        if let JsonVal::Str(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }
    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }
    fn get_u64(&self, key: &str) -> Option<u64> {
        if let JsonVal::Num(n) = self.get(key)? {
            Some((*n).max(0.0) as u64)
        } else {
            None
        }
    }
    fn get_bool(&self, key: &str) -> Option<bool> {
        if let JsonVal::Bool(b) = self.get(key)? {
            Some(*b)
        } else {
            None
        }
    }
    fn get_arr(&self, key: &str) -> Option<&[JsonVal]> {
        if let JsonVal::Arr(v) = self.get(key)? {
            Some(v.as_slice())
        } else {
            None
        }
    }
    fn get_obj(&self, key: &str) -> Option<&[(String, JsonVal)]> {
        if let JsonVal::Obj(v) = self.get(key)? {
            Some(v.as_slice())
        } else {
            None
        }
    }
}

struct JsonParser<'a> {
    b: &'a [u8],
    pos: usize,
}

fn parse_json(s: &str) -> Option<JsonVal> {
    let mut p = JsonParser {
        b: s.as_bytes(),
        pos: 0,
    };
    p.parse_val(0)
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }

    fn parse_val(&mut self, depth: u32) -> Option<JsonVal> {
        if depth > 32 {
            return None;
        }
        self.skip_ws();
        let ch = *self.b.get(self.pos)?;
        match ch {
            b'"' => Some(JsonVal::Str(self.parse_str()?)),
            b'{' => self.parse_obj(depth + 1),
            b'[' => self.parse_arr(depth + 1),
            b't' => {
                self.pos = (self.pos + 4).min(self.b.len());
                Some(JsonVal::Bool(true))
            }
            b'f' => {
                self.pos = (self.pos + 5).min(self.b.len());
                Some(JsonVal::Bool(false))
            }
            b'n' => {
                self.pos = (self.pos + 4).min(self.b.len());
                Some(JsonVal::Null)
            }
            b'-' | b'0'..=b'9' => self.parse_num(),
            _ => None,
        }
    }

    fn parse_str(&mut self) -> Option<String> {
        if *self.b.get(self.pos)? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            self.pos += 1;
            if c == b'"' {
                return Some(out);
            }
            if c == b'\\' && self.pos < self.b.len() {
                let esc = self.b[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' if self.pos + 4 <= self.b.len() => {
                        let hex = core::str::from_utf8(&self.b[self.pos..self.pos + 4]).ok()?;
                        if let Ok(cp) = u32::from_str_radix(hex, 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                            }
                        }
                        self.pos += 4;
                    }
                    other => out.push(other as char),
                }
            } else if c < 0x80 {
                out.push(c as char);
            } else {
                // Multi-byte UTF-8 passthrough
                let start = self.pos - 1;
                let mut end = self.pos;
                while end < self.b.len() && (self.b[end] & 0xC0) == 0x80 {
                    end += 1;
                }
                if let Ok(s) = core::str::from_utf8(&self.b[start..end]) {
                    out.push_str(s);
                }
                self.pos = end;
            }
        }
        None
    }

    fn parse_num(&mut self) -> Option<JsonVal> {
        let start = self.pos;
        while self.pos < self.b.len()
            && matches!(self.b[self.pos], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.pos += 1;
        }
        let s = core::str::from_utf8(&self.b[start..self.pos]).ok()?;
        Some(JsonVal::Num(s.parse::<f64>().ok()?))
    }

    fn parse_arr(&mut self, depth: u32) -> Option<JsonVal> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if *self.b.get(self.pos)? == b']' {
                self.pos += 1;
                return Some(JsonVal::Arr(items));
            }
            items.push(self.parse_val(depth)?);
            self.skip_ws();
            match *self.b.get(self.pos)? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(JsonVal::Arr(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_obj(&mut self, depth: u32) -> Option<JsonVal> {
        self.pos += 1;
        let mut entries = Vec::new();
        loop {
            self.skip_ws();
            if *self.b.get(self.pos)? == b'}' {
                self.pos += 1;
                return Some(JsonVal::Obj(entries));
            }
            let k = self.parse_str()?;
            self.skip_ws();
            if *self.b.get(self.pos)? != b':' {
                return None;
            }
            self.pos += 1;
            let v = self.parse_val(depth)?;
            entries.push((k, v));
            self.skip_ws();
            match *self.b.get(self.pos)? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(JsonVal::Obj(entries));
                }
                _ => return None,
            }
        }
    }
}

fn parse_pc_key(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u64>().ok()
    }
}
