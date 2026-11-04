//! F3's debug overlay, Minecraft's: a `hn.Stats` layer over the main card (scene/10-harness.flecs) with a monospace
//! caption and a 120-frame frame-time sparkline, toggled by `act.ToggleStats` (scene/20-keys.flecs) and kept in
//! native.json `stats`.
//!
//! Hidden it costs nothing beyond a few counters that are always kept because they are trivially cheap: the last poke's
//! reason (a tag, or the line that poked), the glyph and shape caches' hit counts, the GPU time the frame's completion
//! handler already has, the link's round trip and byte counts. Shown, each frame also pushes its CPU build time, GPU time
//! and interval onto rings, and about every [`TICK`] seconds the caption is written again (entity counts, memory and the
//! rest are read then, never per frame). While the app is otherwise idle the window wakes it at that rate and no faster
//! (src/window.rs re-arms a 250 ms timer while [`App::stats_arm`] says so); a tick's own redraw never counts as one.
use super::*;
use crate::pane::{TermFocus, TermGrid};
use harness_native_ui_node_api::Node;
use std::panic::Location;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Frames the rings and the sparkline hold.
const RING: usize = 120;
/// How often the caption is written again while shown (about 4 Hz).
pub const TICK: Duration = Duration::from_millis(250);
/// The typesetter's poke while it settles a new caption: not a reason to redraw of its own.
const LAYOUT: &str = "layout";

/// What asked for the last redraw: a tag where the poke names one, else the line that poked.
#[derive(Clone, Copy, Debug, Default)]
pub enum Why {
    #[default]
    None,
    Tag(&'static str),
    At(&'static Location<'static>),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("-"),
            Self::Tag(t) => f.write_str(t),
            Self::At(l) => {
                let file = l.file();
                let file = file.rsplit_once("/src/").map_or(file, |(_, f)| f);
                write!(f, "{file}:{}", l.line())
            }
        }
    }
}

/// The sparkline's node: the host draws the frame-time bars into its rect (its Fill the bars, its `hot` child's Fill a
/// frame over `budget_ms`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct StatsSpark {
    pub budget_ms: f32,
}

pub(super) fn register(world: &World) {
    world.component::<StatsSpark>().writable();
}

/// The last [`RING`] values pushed, oldest first.
#[derive(Clone)]
struct Ring {
    v: [f32; RING],
    n: usize,
    at: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Self { v: [0.0; RING], n: 0, at: 0 }
    }
}

impl Ring {
    fn push(&mut self, x: f32) {
        self.v[self.at] = x;
        self.at = (self.at + 1) % RING;
        self.n = (self.n + 1).min(RING);
    }

    fn last(&self) -> f32 {
        if self.n == 0 { 0.0 } else { self.v[(self.at + RING - 1) % RING] }
    }

    fn ordered(&self) -> Vec<f32> {
        (0..self.n).map(|i| self.v[(self.at + RING - self.n + i) % RING]).collect()
    }

    /// The `q` quantile (0 to 1) of what it holds.
    fn quantile(&self, q: f32) -> f32 {
        let mut v = self.ordered();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f32::total_cmp);
        v[((v.len() - 1) as f32 * q).round() as usize]
    }
}

/// What the overlay rates against, as of the last tick.
#[derive(Clone, Copy, Default)]
struct Prev {
    glyph: (u64, u64),
    shape: (u64, u64),
    /// The shown object's stream (its key), its byte count, and when.
    pty: Option<(u64, u64, Instant)>,
}

#[derive(Default)]
pub(super) struct Stats {
    pub shown: bool,
    /// The `hn.Stats` node while shown.
    node: Option<u64>,
    /// The last poke's reason (always kept: a copy).
    pub why: Why,
    /// While shown: when a poke other than the typesetter settling the caption last came, and how many came since the
    /// tick being drawn began.
    last_real: Option<(Instant, Why)>,
    real: u32,
    /// A real poke came since the last frame ended (the frame it asked for).
    asked: bool,
    /// A 250 ms wake is scheduled (src/window.rs).
    armed: bool,
    begun: Option<Instant>,
    last_begin: Option<Instant>,
    /// The tick being drawn: the pending frames and reason before it, restored when only the caption moved.
    tick: Option<(u32, Why)>,
    last_tick: Option<Instant>,
    cpu: Ring,
    gpu: Ring,
    dt: Ring,
    /// Frame starts in the last second (the 1 s average), each with whether something asked for it.
    stamps: std::collections::VecDeque<(Instant, bool)>,
    /// The last frame's instances: panels, glyphs, sprites.
    draws: [usize; 3],
    gpu_clock: Option<Arc<AtomicU32>>,
    prev: Prev,
    /// The focused terminal's scrollback lines, as its last draw read them (only while shown).
    pub history: usize,
    /// The sparkline as of the last tick (CPU + GPU milliseconds a frame, oldest first).
    spark: Vec<f32>,
}

impl Stats {
    /// A poke: its reason, and (while shown) whether it was a real one.
    pub fn note(&mut self, why: Why) {
        self.why = why;
        if self.shown && !matches!(why, Why::Tag(LAYOUT)) {
            self.real += 1;
            self.asked = true;
            self.last_real = Some((Instant::now(), why));
        }
    }
}

fn rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: task_info writes at most `count` integers of MACH_TASK_BASIC_INFO into `info`, which is that struct.
    #[allow(deprecated)]
    let kr = unsafe { libc::task_info(libc::mach_task_self(), libc::MACH_TASK_BASIC_INFO, info.as_mut_ptr().cast(), &mut count) };
    // SAFETY: task_info succeeded, so it filled the struct (zeroed before in any case).
    (kr == 0).then(|| unsafe { info.assume_init() }.resident_size)
}

fn mb(b: f64) -> String {
    if b >= 1024.0 * 1024.0 { format!("{:.1} MB", b / (1024.0 * 1024.0)) } else if b >= 1024.0 { format!("{:.1} KB", b / 1024.0) } else { format!("{b:.0} B") }
}

fn rate(d: (u64, u64)) -> String {
    let n = d.0 + d.1;
    if n == 0 { "-".into() } else { format!("{:.1}%", d.0 as f64 * 100.0 / n as f64) }
}

impl App {
    pub fn stats_shown(&self) -> bool {
        self.stats.shown
    }

    /// F3: show the overlay or take it away (kept in native.json, except for the offscreen shot).
    pub(super) fn toggle_stats(&mut self) {
        self.set_stats(!self.stats.shown);
        if self.persist {
            crate::prefs::set("stats", serde_json::json!(self.stats.shown));
        }
    }

    /// Show the overlay or not. Hiding drops the node and every ring, so a hidden overlay holds nothing.
    pub fn set_stats(&mut self, on: bool) {
        if on == self.stats.shown {
            return;
        }
        let w = self.world.world_ref();
        if on {
            let node = View::of(w.entity_from_id(self.ids.main)).child("hn.Stats");
            let (why, gpu) = (self.stats.why, self.stats.gpu_clock.take());
            self.stats = Stats { shown: true, node: Some(node.id()), why, gpu_clock: gpu, ..Stats::default() };
        } else {
            if let Some(n) = self.stats.node.take() {
                w.entity_from_id(n).destruct();
            }
            let (why, gpu) = (self.stats.why, self.stats.gpu_clock.take());
            self.stats = Stats { why, gpu_clock: gpu, ..Stats::default() };
        }
        self.poke_why("stats toggled");
    }

    /// Where the renderer leaves each frame's GPU time (src/gpu.rs).
    pub fn set_gpu_clock(&mut self, c: Arc<AtomicU32>) {
        self.stats.gpu_clock = Some(c);
    }

    /// Whether the window should schedule a wake in [`TICK`] (the overlay is shown, the app idle, none scheduled yet).
    pub fn stats_arm(&mut self) -> bool {
        if self.stats.shown && !self.stats.armed {
            self.stats.armed = true;
            return true;
        }
        false
    }

    /// The scheduled wake came: one frame, without a reason of its own.
    pub fn stats_wake(&mut self) {
        self.stats.armed = false;
        if self.stats.shown {
            self.pending = self.pending.max(1);
        }
    }

    /// A frame begins (after its pending count went down): the interval is pushed and, once a tick is due, the caption
    /// written again.
    pub(super) fn stats_begin(&mut self) {
        if !self.stats.shown {
            return;
        }
        let now = Instant::now();
        let s = &mut self.stats;
        if let Some(l) = s.last_begin {
            s.dt.push((now - l).as_secs_f32() * 1000.0);
        }
        s.last_begin = Some(now);
        s.begun = Some(now);
        s.stamps.push_back((now, false));
        while s.stamps.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(1)) {
            s.stamps.pop_front();
        }
        if s.last_tick.is_some_and(|t| now - t < TICK) {
            return;
        }
        s.last_tick = Some(now);
        s.tick = Some((self.pending, s.why));
        s.real = 0;
        s.spark = s.cpu.ordered().iter().zip(s.gpu.ordered()).map(|(c, g)| c + g).collect();
        let text = self.stats_text(now);
        let top = self.chrome.strip / self.zoom();
        if let Some(n) = self.stats.node.map(|n| View::of(self.world.entity_from_id(n))) {
            if let Some(t) = n.part("plate").and_then(|p| p.part("text")) {
                t.text(&text);
            }
            // An Overlay child takes its parent's whole frame, not its safe area: pad it down past the title-bar strip.
            let proto = view::prefab(self.world.world_ref(), "hn.Stats").and_then(|p| p.try_cloned::<&harness_native_ui_layout_api::Padding>()).unwrap_or_default();
            let pad = harness_native_ui_layout_api::Padding { t: proto.t + top, ..proto };
            if n.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(pad) {
                n.set(pad);
            }
        }
    }

    /// A frame was built into `d`: its CPU time, the last GPU time and its instances. A tick whose only redraw was its
    /// own caption leaves the pending frames and the reason as they were.
    pub(super) fn stats_end(&mut self, d: &Draw) {
        if !self.stats.shown {
            return;
        }
        let s = &mut self.stats;
        if let Some(b) = s.begun.take() {
            s.cpu.push(b.elapsed().as_secs_f32() * 1000.0);
        }
        s.gpu.push(s.gpu_clock.as_ref().map_or(0.0, |c| f32::from_bits(c.load(Ordering::Relaxed))));
        s.draws = [d.panels.len(), d.glyphs.len(), d.sprites.len()];
        if let Some(last) = s.stamps.back_mut() {
            last.1 = s.asked;
        }
        s.asked = false;
        if let Some((pending, why)) = s.tick.take()
            && s.real == 0
        {
            self.pending = pending;
            self.stats.why = why;
        }
    }

    /// The caption: one fact a line, columns padded so it holds still in the monospace face.
    fn stats_text(&mut self, now: Instant) -> String {
        let w = self.world.world_ref();
        let mut out = Vec::new();
        let nodes = self.world.new_query::<&Node>().count();
        let laid = self.world.new_query::<(&Node, &Laid)>().count();
        let s = &self.stats;
        let inst = s.dt.last();
        let fps_now = if inst > 0.0 { 1000.0 / inst } else { 0.0 };
        out.push(format!("fps   {fps_now:>5.0} now  {:>5} in 1s", s.stamps.len()));
        out.push(format!("cpu   {:>5.2} ms  p50 {:>5.2}  p99 {:>5.2}", s.cpu.last(), s.cpu.quantile(0.5), s.cpu.quantile(0.99)));
        out.push(format!("gpu   {:>5.2} ms  p50 {:>5.2}  p99 {:>5.2}", s.gpu.last(), s.gpu.quantile(0.5), s.gpu.quantile(0.99)));
        // Redrawing while something other than this overlay's tick keeps asking for frames (20 or more in the last second:
        // an animation, a stream); the daemon's 400 ms list poll alone (3 frames each) reads as idle.
        let asked = s.stamps.iter().filter(|(_, a)| *a).count();
        let busy = asked >= 20 && s.last_real.is_some_and(|(t, _)| now - t < TICK + Duration::from_millis(50));
        let state = if busy {
            "redrawing"
        } else if self.breathing() {
            "idle (breathing)"
        } else {
            "idle"
        };
        out.push(format!("state {state}  ({asked} of {} frames asked for in 1s)", s.stamps.len()));
        let last = match s.last_real {
            Some((t, why)) => format!("{why}  {:.1}s ago", (now - t).as_secs_f32()),
            None => format!("{}", s.why),
        };
        out.push(format!("why   {last}"));
        out.push(format!("ecs  {} entities  {nodes} nodes  {laid} laid", w.alive_count()));
        out.push(format!("draw  {} panels  {} glyphs  {} sprites", s.draws[0], s.draws[1], s.draws[2]));
        let glyph = (self.cache.hits, self.cache.misses);
        let shape = (self.grid.shape_hits, self.grid.shape_misses);
        let d = |a: (u64, u64), b: (u64, u64)| (a.0.saturating_sub(b.0), a.1.saturating_sub(b.1));
        out.push(format!("atlas {:.1}% full  glyph hit {}  shape hit {}", self.cache.fill() * 100.0, rate(d(glyph, s.prev.glyph)), rate(d(shape, s.prev.shape))));
        // The shown terminal: its object, grid, cell in device pixels, scrollback and stream rate.
        let mut pty = None;
        let term = self.focused_term().map(|t| {
            let te = w.entity_from_id(t);
            (te.try_cloned::<&TermGrid>().unwrap_or_default(), te.parent().and_then(|p| model::shows(w, *p.id())).map(|e| *e.id()))
        });
        match term {
            Some((g, Some(e))) if g.cols > 0 => {
                let (label, key, bytes) = match (self.session_of(e), self.run_of(e)) {
                    (Some(sid), _) => (format!("s{sid}"), sid, self.link.session_bytes(sid)),
                    (None, Some((n, k))) => (k.map_or(format!("r{n}"), |k| format!("r{n}.{k}")), n << 8 | u64::from(k.map_or(255, |k| k.min(254) as u8)), self.link.run_bytes(n, k)),
                    _ => ("-".into(), 0, 0),
                };
                let bps = match s.prev.pty {
                    Some((k, b, t)) if k == key && now > t => bytes.saturating_sub(b) as f64 / (now - t).as_secs_f64(),
                    _ => 0.0,
                };
                pty = Some((key, bytes, now));
                let (cw, ch) = ((self.grid.cw * self.grid.scale).round(), (self.grid.ch * self.grid.scale).round());
                out.push(format!("term  {label} {}x{}  cell {cw}x{ch}px  sb {}", g.cols, g.rows, s.history));
                out.push(format!("pty   {}/s  ({} total)", mb(bps), mb(bytes as f64)));
            }
            _ => out.push("term  -".into()),
        }
        let link = w.lookup(model::LOCAL).target(model::Link::id(), 0).map_or(0, |t| *t.id());
        let state = if link == *w.component_id::<model::Up>() {
            "up"
        } else if link == *w.component_id::<model::Down>() {
            "down"
        } else {
            "connecting"
        };
        let rtt = self.link.rtt_ms().map_or("-".into(), |r| format!("{r:.2} ms"));
        let pid = self.link.daemon_pid().map_or("-".into(), |p| p.to_string());
        out.push(format!("link  {state}  rtt {rtt}  harnessd {pid}"));
        out.push(format!("mem   rss {}", rss_bytes().map_or("-".into(), |b| mb(b as f64))));
        self.stats.prev = Prev { glyph, shape, pty };
        out.join("\n")
    }

    /// The sparkline: one bar a frame (newest at the right), as tall as its CPU + GPU time against twice the budget, in
    /// the node's Fill (its `hot` child's over budget), with a faint line at the budget.
    pub(super) fn emit_spark(&self, d: &mut Draw, e: EntityView<'_>, r: [f32; 4], alpha: f32) {
        let pick = |f: Option<Fill>| f.map_or([1.0; 4], |f| rgba(if self.dark { f.dark } else { f.light }));
        let ok = pick(e.try_cloned::<&Fill>());
        let hot = pick(e.lookup_child("hot").and_then(|h| h.try_cloned::<&Fill>()));
        let budget = e.try_cloned::<&StatsSpark>().map_or(16.67, |s| s.budget_ms).max(0.1);
        let flat = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
        let bw = r[2] / RING as f32;
        let full = 2.0 * budget;
        let n = self.stats.spark.len();
        for (i, v) in self.stats.spark.iter().enumerate() {
            let h = (v / full).clamp(0.0, 1.0) * r[3];
            let x = r[0] + r[2] - (n - i) as f32 * bw;
            d.panel(panel_from(if *v > budget { hot } else { ok }, [0.0; 4], flat, [x, r[1] + r[3] - h.max(0.5), (bw - 0.5).max(0.5), h.max(0.5)], alpha));
        }
        let line = [ok[0], ok[1], ok[2], ok[3] * 0.35];
        d.panel(panel_from(line, [0.0; 4], flat, [r[0], r[1] + r[3] * 0.5, r[2], 0.5], alpha));
    }

    /// The focused terminal's scrollback lines, from its draw (the parser's one reader) while the overlay is shown.
    pub(super) fn stats_history(&mut self, t: u64, lines: usize) {
        if self.stats.shown && self.world.entity_from_id(t).has(TermFocus::id()) {
            self.stats.history = lines;
        }
    }
}
