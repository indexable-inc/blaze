//! Zero-dependency Rust -> `wasm32-unknown-unknown` core for `ix.dev/blaze/explore`.
//!
//! Directly includes `crates/blaze-launch/src/telemetry.rs` (zero schema duplication!) so the
//! browser decodes the exact 64-byte `#[repr(C, align(8))] Event` emitted by `blaze-launcher`,
//! tracks per-CPU core utilization + cross-core thread migrations, and generates packed WebGPU
//! NDC instance buffers directly in WASM linear memory for zero-copy `queue.writeBuffer`.

#![allow(static_mut_refs)]

extern crate self as blaze_own;

/// Minimal zero-dependency shim for `blaze_own::planted` so `crates/blaze-launch/src/telemetry.rs`
/// compiles directly into `wasm32-unknown-unknown` with zero schema duplication.
pub mod planted {
    pub trait Plants: Copy + Eq {}
    pub trait Planted<B: Plants>: Copy {
        fn has(self, bug: B) -> bool;
    }
    #[derive(Clone, Copy, Debug, Default)]
    pub struct NoPlant;
    impl<B: Plants> Planted<B> for NoPlant {
        #[inline]
        fn has(self, _bug: B) -> bool {
            false
        }
    }
}

/// Matches `blaze_launch::Slot` for shared `telemetry.rs` inclusion.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Slot(pub u32);

#[path = "../../../../crates/blaze-launch/src/telemetry.rs"]
pub mod telemetry;

use telemetry::{CPU_UNKNOWN, Event, EventKind};

const MAX_SAMPLES: usize = 16_384;
const MAX_RECTS: usize = 16_384;
const MAX_INGEST_BYTES: usize = 65_536;
const MAX_CPUS: usize = 128;
const MAX_THREADS: usize = 64;

#[derive(Clone, Copy)]
#[repr(C)]
struct Sample {
    start_ms: f64,
    end_ms: f64,
    kind: u8,
    depth: u8,
    flags: u8,
    _pad: u8,
    cpu: u16,
    _pad2: u16,
    thread: u32,
    slot_or_op: u32,
    holder_thread: u32,
    value_a: u64,
    value_b: u64,
    value_c: u64,
}

impl Sample {
    const EMPTY: Self = Self {
        start_ms: 0.0,
        end_ms: 0.0,
        kind: 0,
        depth: 0,
        flags: 0,
        _pad: 0,
        cpu: 0,
        _pad2: 0,
        thread: 0,
        slot_or_op: 0,
        holder_thread: 0,
        value_a: 0,
        value_b: 0,
        value_c: 0,
    };
}

#[derive(Clone, Copy)]
struct ThreadTrack {
    tid: u32,
    last_cpu: u16,
    migrations: u32,
    events: u32,
}

impl ThreadTrack {
    const EMPTY: Self = Self { tid: 0, last_cpu: 0, migrations: 0, events: 0 };
}

static mut INGEST_BUF: [u8; MAX_INGEST_BYTES] = [0; MAX_INGEST_BYTES];
static mut SAMPLES: [Sample; MAX_SAMPLES] = [Sample::EMPTY; MAX_SAMPLES];
static mut SAMPLE_HEAD: usize = 0;
static mut SAMPLE_LEN: usize = 0;

/// Packed WebGPU instance buffer: `[x0, y0, x1, y1, r, g, b, a]` per rect in NDC.
static mut GPU_RECTS: [f32; MAX_RECTS * 8] = [0.0; MAX_RECTS * 8];
static mut GPU_RECT_COUNT: usize = 0;

/// Per-CPU event/duration counters + thread migration tracking.
static mut CPU_EVENTS: [u32; MAX_CPUS] = [0; MAX_CPUS];
static mut CPU_BUSY_US: [u32; MAX_CPUS] = [0; MAX_CPUS];
static mut THREADS: [ThreadTrack; MAX_THREADS] = [ThreadTrack::EMPTY; MAX_THREADS];
static mut THREAD_COUNT: usize = 0;
static mut TOTAL_MIGRATIONS: u32 = 0;

#[unsafe(no_mangle)]
pub extern "C" fn ingest_buf_ptr() -> *mut u8 {
    unsafe { INGEST_BUF.as_mut_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_buf_cap() -> usize {
    MAX_INGEST_BYTES
}

#[unsafe(no_mangle)]
pub extern "C" fn gpu_rects_ptr() -> *const f32 {
    unsafe { GPU_RECTS.as_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn cpu_events_ptr() -> *const u32 {
    unsafe { CPU_EVENTS.as_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn cpu_busy_us_ptr() -> *const u32 {
    unsafe { CPU_BUSY_US.as_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn total_migrations() -> u32 {
    unsafe { TOTAL_MIGRATIONS }
}

#[unsafe(no_mangle)]
pub extern "C" fn sample_count() -> u32 {
    unsafe { SAMPLE_LEN as u32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn clear_state() {
    unsafe {
        SAMPLE_HEAD = 0;
        SAMPLE_LEN = 0;
        GPU_RECT_COUNT = 0;
        THREAD_COUNT = 0;
        TOTAL_MIGRATIONS = 0;
        for c in &mut CPU_EVENTS {
            *c = 0;
        }
        for c in &mut CPU_BUSY_US {
            *c = 0;
        }
        for t in &mut THREADS {
            *t = ThreadTrack::EMPTY;
        }
    }
}

fn note_thread_cpu(tid: u32, cpu: u16, dur_us: u32) {
    if tid == 0 {
        return;
    }
    unsafe {
        if (cpu as usize) < MAX_CPUS {
            CPU_EVENTS[cpu as usize] = CPU_EVENTS[cpu as usize].saturating_add(1);
            CPU_BUSY_US[cpu as usize] = CPU_BUSY_US[cpu as usize].saturating_add(dur_us);
        }
        for i in 0..THREAD_COUNT {
            if THREADS[i].tid == tid {
                THREADS[i].events = THREADS[i].events.saturating_add(1);
                if cpu != CPU_UNKNOWN {
                    if THREADS[i].last_cpu != CPU_UNKNOWN && THREADS[i].last_cpu != cpu {
                        THREADS[i].migrations = THREADS[i].migrations.saturating_add(1);
                        TOTAL_MIGRATIONS = TOTAL_MIGRATIONS.saturating_add(1);
                    }
                    THREADS[i].last_cpu = cpu;
                }
                return;
            }
        }
        if THREAD_COUNT < MAX_THREADS {
            THREADS[THREAD_COUNT] = ThreadTrack { tid, last_cpu: cpu, migrations: 0, events: 1 };
            THREAD_COUNT += 1;
        }
    }
}

/// Ingests `byte_len` packed 64-byte `blaze_launch::telemetry::Event` records from `INGEST_BUF`.
#[unsafe(no_mangle)]
pub extern "C" fn ingest_events(byte_len: usize, now_ms: f64) -> u32 {
    let clamped = byte_len.min(MAX_INGEST_BYTES);
    let n = clamped / 64;
    let mut ingested = 0u32;
    for i in 0..n {
        let mut raw = [0u8; 64];
        unsafe {
            core::ptr::copy_nonoverlapping(INGEST_BUF.as_ptr().add(i * 64), raw.as_mut_ptr(), 64);
        }
        // SAFETY: `Event` is `#[repr(C, align(8))]`, 64 bytes of plain integers.
        let ev: Event = unsafe { core::mem::transmute(raw) };
        let Some(kind) = ev.event_kind() else {
            continue;
        };

        let dur_ms = match kind {
            EventKind::SpanExit | EventKind::SpanUnwound => (ev.a as f64 / 1_000_000.0).max(0.25),
            EventKind::Io | EventKind::SyncWait => (ev.a as f64 / 1_000_000.0).max(4.0),
            EventKind::Alloc => 10.0,
            _ => continue,
        };
        let dur_us = (ev.a / 1_000).min(u32::MAX as u64) as u32;
        note_thread_cpu(ev.thread, ev.cpu, dur_us);

        let depth = match kind {
            EventKind::SpanExit | EventKind::SpanUnwound => {
                if ev.c != 0 {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        };
        let holder_thread = if kind == EventKind::SyncWait { ev.d as u32 } else { 0 };
        let sample = Sample {
            start_ms: now_ms - dur_ms,
            end_ms: now_ms,
            kind: ev.kind,
            depth,
            flags: ev.flags,
            _pad: 0,
            cpu: ev.cpu,
            _pad2: 0,
            thread: ev.thread,
            slot_or_op: ev.slot_or_op,
            holder_thread,
            value_a: ev.a,
            value_b: ev.b,
            value_c: ev.c,
        };
        unsafe {
            SAMPLES[SAMPLE_HEAD] = sample;
            SAMPLE_HEAD = (SAMPLE_HEAD + 1) & (MAX_SAMPLES - 1);
            if SAMPLE_LEN < MAX_SAMPLES {
                SAMPLE_LEN += 1;
            }
        }
        ingested += 1;
    }
    ingested
}

/// Builds WebGPU NDC rects for the active viewport directly into `GPU_RECTS` in WASM linear memory.
/// Returns the number of rects written (`count * 8` f32s starting at `gpu_rects_ptr()`).
#[unsafe(no_mangle)]
pub extern "C" fn build_gpu_rects(
    view_start_ms: f64,
    view_end_ms: f64,
    css_w: f32,
    css_h: f32,
    scroll_y: f32,
    gutter_w: f32,
) -> u32 {
    let span_ms = (view_end_ms - view_start_ms).max(0.01);
    let plot_w = (css_w - gutter_w).max(1.0);
    let mut count = 0usize;

    let mut push = |x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32| {
        if count >= MAX_RECTS || w <= 0.0 || h <= 0.0 || y + h < 0.0 || y > css_h {
            return;
        }
        let o = count * 8;
        let x0 = -1.0 + (x / css_w) * 2.0;
        let x1 = -1.0 + ((x + w) / css_w) * 2.0;
        let y0 = 1.0 - ((y + h) / css_h) * 2.0;
        let y1 = 1.0 - (y / css_h) * 2.0;
        unsafe {
            GPU_RECTS[o] = x0;
            GPU_RECTS[o + 1] = y0;
            GPU_RECTS[o + 2] = x1;
            GPU_RECTS[o + 3] = y1;
            GPU_RECTS[o + 4] = r;
            GPU_RECTS[o + 5] = g;
            GPU_RECTS[o + 6] = b;
            GPU_RECTS[o + 7] = a;
        }
        count += 1;
    };

    unsafe {
        let t_count = THREAD_COUNT.max(1);
        let row_h = 56.0f32;
        let heap_y = (t_count as f32) * row_h - scroll_y;
        let io_y = heap_y + 38.0;

        let total = SAMPLE_LEN;
        let start_idx = if SAMPLE_LEN == MAX_SAMPLES { SAMPLE_HEAD } else { 0 };
        for i in 0..total {
            let s = SAMPLES[(start_idx + i) & (MAX_SAMPLES - 1)];
            if s.end_ms < view_start_ms || s.start_ms > view_end_ms {
                continue;
            }
            let raw_x0 = gutter_w + (((s.start_ms - view_start_ms) / span_ms) as f32) * plot_w;
            let raw_x1 = gutter_w + (((s.end_ms - view_start_ms) / span_ms) as f32) * plot_w;
            let x0 = raw_x0.max(gutter_w + 1.0);
            let x1 = raw_x1.max(x0 + 3.0).min(css_w);
            let w = x1 - x0;

            let (y, h, r, g, b, a) = match EventKind::from_raw(s.kind) {
                Some(EventKind::SpanExit | EventKind::SpanUnwound) => {
                    let mut ti = 0usize;
                    for k in 0..THREAD_COUNT {
                        if THREADS[k].tid == s.thread {
                            ti = k;
                            break;
                        }
                    }
                    let base_y = (ti as f32) * row_h - scroll_y + 6.0 + (s.depth as f32) * 23.0;
                    let hot = s.value_a >= 100_000_000;
                    if hot {
                        (base_y, 20.0, 0.85, 0.28, 0.11, 0.92)
                    } else if s.depth > 0 {
                        (base_y, 20.0, 0.16, 0.49, 0.82, 0.88)
                    } else {
                        (base_y, 20.0, 0.11, 0.56, 0.41, 0.88)
                    }
                }
                Some(EventKind::Alloc) => (heap_y + 6.0, 22.0, 0.22, 0.52, 0.88, 0.88),
                Some(EventKind::SyncWait) => {
                    // Draw vertical dependency connector from holder thread -> lock stall!
                    if s.holder_thread != 0 {
                        for k in 0..THREAD_COUNT {
                            if THREADS[k].tid == s.holder_thread {
                                let holder_y = (k as f32) * row_h - scroll_y + 16.0;
                                let top = holder_y.min(io_y + 6.0);
                                let bot = holder_y.max(io_y + 6.0);
                                push(x0 + w * 0.5, top, 1.5, bot - top, 0.95, 0.38, 0.35, 0.48);
                                break;
                            }
                        }
                    }
                    (io_y + 6.0, 20.0, 0.95, 0.38, 0.35, 0.92)
                }
                Some(EventKind::Io) => (io_y + 6.0, 20.0, 0.85, 0.58, 0.19, 0.88),
                _ => continue,
            };
            push(x0, y, w, h, r, g, b, a);
        }
        GPU_RECT_COUNT = count;
    }
    count as u32
}
