//! The recorder: a synchronous state machine (rule sans-io-cores). It owns no thread, file,
//! socket or clock: drivers (the tracing layer, the sampler thread, the allocator, the HTTP
//! server) hand it events stamped with times they read, and take back profiles
//! ([`Recorder::snapshot`]) and live deltas ([`Recorder::take_live`]). Every table is held in
//! a bounded ring; what falls out is counted ([`Recorder::dropped`]), never silent.

use std::collections::{BTreeMap, VecDeque};

use crate::profile::{
    Alloc, AllocEvent, Counter, CpuSample, Frame, FrameKind, Interner, Meta, Profile, Span,
    SpanKind, Thread, join_fields,
};

/// How much the recorder keeps.
#[derive(Clone, Debug)]
pub struct Limits {
    pub spans: usize,
    pub samples: usize,
    pub alloc_events: usize,
    pub counters: usize,
    /// Open spans; past this, new spans are dropped (and counted) until some close.
    pub open_spans: usize,
    /// Distinct allocation stacks aggregated; past this, new stacks fold into one row.
    pub alloc_stacks: usize,
    /// Sampled blocks tracked for live bytes.
    pub live_blocks: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            spans: 1 << 20,
            samples: 1 << 20,
            alloc_events: 1 << 18,
            counters: 1 << 18,
            open_spans: 1 << 16,
            alloc_stacks: 1 << 16,
            live_blocks: 1 << 18,
        }
    }
}

/// A span as it is made.
#[derive(Clone, Debug, Default)]
pub struct NewSpan {
    pub id: u64,
    pub parent: Option<u64>,
    pub thread: u64,
    pub name: String,
    pub target: String,
    pub kind: Option<SpanKind>,
    pub task: Option<u64>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub wait: Option<String>,
    pub wait_on: Option<String>,
    pub wait_owner: Option<String>,
    pub fields: Vec<(String, String)>,
}

/// What a thread had used when it entered or left a span: its CPU time and what it allocated
/// (bytes, count, bytes freed), each when the driver can read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu_ns: Option<i64>,
    pub alloc_bytes: Option<u64>,
    pub allocs: Option<u64>,
    pub freed_bytes: Option<u64>,
}

/// A CPU sample as the sampler takes it: return addresses, innermost first, the first the
/// interrupted PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawSample {
    pub thread: u64,
    pub t_ns: i64,
    pub pcs: Vec<u64>,
    pub span_id: Option<u64>,
    pub on_cpu: bool,
    pub weight_ns: u64,
}

/// A sampled allocation: the block's address, what it stands for (already scaled), its stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawAlloc {
    pub thread: u64,
    pub t_ns: i64,
    pub address: u64,
    pub bytes: u64,
    pub count: u64,
    pub pcs: Vec<u64>,
    pub span_id: Option<u64>,
}

/// Names addresses: each address's frames, the function its code belongs to first and the
/// functions inlined into it after (inline_depth 0, 1, ...).
pub trait Symbolize {
    fn symbolize(&mut self, addresses: &[u64]) -> Vec<Vec<Frame>>;
    fn symbolized(&self) -> bool;
}

/// Addresses only: what a reader symbolizes later.
pub struct NoSymbols;

impl Symbolize for NoSymbols {
    fn symbolize(&mut self, addresses: &[u64]) -> Vec<Vec<Frame>> {
        addresses
            .iter()
            .map(|a| {
                let mut f = Frame::named(format!("{a:#x}"), FrameKind::Native);
                f.address = Some(*a);
                vec![f]
            })
            .collect()
    }

    fn symbolized(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
struct Open {
    span: Span,
    entered: u32,
}

#[derive(Clone, Copy, Debug)]
struct Entered {
    id: u64,
    at_ns: i64,
    usage: Usage,
    child_cpu: i64,
}

/// A bounded ring that numbers what it ever held, so a reader can ask "since n".
#[derive(Clone, Debug)]
struct Ring<T> {
    items: VecDeque<T>,
    /// The number of the first item held.
    base: u64,
    cap: usize,
}

impl<T> Ring<T> {
    fn new(cap: usize) -> Ring<T> {
        Ring { items: VecDeque::new(), base: 0, cap: cap.max(1) }
    }

    /// Pushes, and says whether the oldest fell out.
    fn push(&mut self, item: T) -> bool {
        let evicted = self.items.len() == self.cap;
        if evicted {
            self.items.pop_front();
            self.base += 1;
        }
        self.items.push_back(item);
        evicted
    }

    fn end(&self) -> u64 {
        self.base + self.items.len() as u64
    }

    /// The items from number `from` on, and how many before them were already gone.
    fn since(&self, from: u64) -> (impl Iterator<Item = &T>, u64) {
        let skip = from.saturating_sub(self.base) as usize;
        (self.items.iter().skip(skip), self.base.saturating_sub(from))
    }
}

#[derive(Clone, Debug, Default)]
struct AllocAgg {
    alloc_count: u64,
    alloc_bytes: u64,
    live_count: u64,
    live_bytes: u64,
    sampled: u64,
}

type AllocKey = (Vec<u64>, Option<String>);

/// What changed since the last [`Recorder::take_live`].
#[derive(Clone, Debug, Default)]
pub struct LiveDelta {
    pub threads: Vec<Thread>,
    pub spans: Vec<Span>,
    pub open_spans: Vec<Span>,
    pub samples: Vec<RawSample>,
    pub alloc_events: Vec<RawAlloc>,
    /// Sampled blocks freed: `bytes` and `count` are what the block stood for.
    pub frees: Vec<RawAlloc>,
    pub counters: Vec<Counter>,
    /// Items that fell out of the rings before the live reader took them, by table.
    pub missed: BTreeMap<&'static str, u64>,
}

pub struct Recorder {
    pub limits: Limits,
    meta: Meta,
    process: u32,
    process_name: String,
    now_ns: i64,
    threads: BTreeMap<u64, Thread>,
    open: BTreeMap<u64, Open>,
    entered: BTreeMap<u64, Vec<Entered>>,
    spans: Ring<Span>,
    samples: Ring<RawSample>,
    alloc_events: Ring<RawAlloc>,
    frees: Ring<RawAlloc>,
    counters: Ring<Counter>,
    allocs: BTreeMap<AllocKey, AllocAgg>,
    live_blocks: BTreeMap<u64, (AllocKey, u64, u64)>,
    dropped: BTreeMap<&'static str, u64>,
    reasons: BTreeMap<&'static str, String>,
    threads_sent: usize,
    live_from: [u64; 5],
}

impl Recorder {
    /// A recorder for one process; `meta` gives what the manifest says (its times are set
    /// from the events).
    pub fn new(process: u32, process_name: &str, meta: Meta, limits: Limits) -> Recorder {
        Recorder {
            spans: Ring::new(limits.spans),
            samples: Ring::new(limits.samples),
            alloc_events: Ring::new(limits.alloc_events),
            frees: Ring::new(limits.alloc_events),
            counters: Ring::new(limits.counters),
            limits,
            now_ns: meta.start_ns,
            meta,
            process,
            process_name: process_name.to_owned(),
            threads: BTreeMap::new(),
            open: BTreeMap::new(),
            entered: BTreeMap::new(),
            allocs: BTreeMap::new(),
            live_blocks: BTreeMap::new(),
            dropped: BTreeMap::new(),
            reasons: BTreeMap::new(),
            threads_sent: 0,
            live_from: [0; 5],
        }
    }

    pub fn advance_clock(&mut self, now_ns: i64) {
        self.now_ns = self.now_ns.max(now_ns);
    }

    pub fn now_ns(&self) -> i64 {
        self.now_ns
    }

    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    /// Counts `n` records of `table` lost, and why.
    pub fn drop_records(&mut self, table: &'static str, n: u64, why: &str) {
        if n == 0 {
            return;
        }
        *self.dropped.entry(table).or_default() += n;
        self.reasons.insert(table, why.to_owned());
    }

    /// Records lost so far by table, cumulative, with the last reason for each.
    pub fn dropped(&self) -> BTreeMap<&'static str, (u64, String)> {
        self.dropped
            .iter()
            .map(|(k, v)| (*k, (*v, self.reasons.get(k).cloned().unwrap_or_default())))
            .collect()
    }

    pub fn thread(&mut self, thread: u64, name: Option<&str>) {
        let entry = self.threads.entry(thread).or_insert_with(|| Thread {
            process: self.process,
            process_name: self.process_name.clone(),
            thread,
            thread_name: None,
            cpu_ns: None,
        });
        if name.is_some() && entry.thread_name.as_deref() != name {
            entry.thread_name = name.map(str::to_owned);
            self.threads_sent = 0;
        }
    }

    pub fn span_new(&mut self, new: NewSpan, at_ns: i64) {
        self.advance_clock(at_ns);
        if self.open.len() >= self.limits.open_spans {
            self.drop_records("spans", 1, "too many open spans");
            return;
        }
        self.thread(new.thread, None);
        let depth = new
            .parent
            .and_then(|p| self.open.get(&p))
            .map_or(0, |p| p.span.depth.saturating_add(1));
        let kind =
            new.kind.unwrap_or(if new.wait.is_some() { SpanKind::Wait } else { SpanKind::Span });
        let mut fields = new.fields;
        let span = Span {
            process: self.process,
            thread: new.thread,
            span_id: new.id,
            parent: new.parent,
            depth,
            name: new.name,
            target: new.target,
            kind,
            task: new.task,
            file: new.file,
            line: new.line,
            start_ns: at_ns,
            dur_ns: 0,
            busy_ns: None,
            self_cpu_ns: None,
            alloc_bytes: None,
            allocs: None,
            frees_bytes: None,
            wait: new.wait,
            wait_on: new.wait_on,
            wait_owner: new.wait_owner,
            fields: join_fields(&mut fields),
        };
        self.open.insert(span.span_id, Open { span, entered: 0 });
    }

    /// More fields of an open span (tracing's `record`).
    pub fn span_record(&mut self, id: u64, mut fields: Vec<(String, String)>) {
        let Some(open) = self.open.get_mut(&id) else { return };
        let mut all: Vec<(String, String)> = open
            .span
            .fields
            .as_deref()
            .unwrap_or_default()
            .split('\x1f')
            .filter(|it| !it.is_empty())
            .filter_map(|it| it.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .filter(|(k, _)| !fields.iter().any(|(nk, _)| nk == k))
            .collect();
        for (k, v) in &fields {
            match k.as_str() {
                "wait" => open.span.wait = Some(v.clone()),
                "on" => open.span.wait_on = Some(v.clone()),
                "owner" => open.span.wait_owner = Some(v.clone()),
                _ => {}
            }
        }
        if open.span.wait.is_some() {
            open.span.kind = SpanKind::Wait;
        }
        fields.retain(|(k, _)| !matches!(k.as_str(), "wait" | "on" | "owner"));
        all.extend(fields);
        open.span.fields = join_fields(&mut all);
    }

    pub fn span_enter(&mut self, id: u64, thread: u64, at_ns: i64, usage: Usage) {
        self.advance_clock(at_ns);
        let Some(open) = self.open.get_mut(&id) else { return };
        open.entered += 1;
        self.entered.entry(thread).or_default().push(Entered { id, at_ns, usage, child_cpu: 0 });
    }

    pub fn span_exit(&mut self, id: u64, thread: u64, at_ns: i64, usage: Usage) {
        self.advance_clock(at_ns);
        let Some(stack) = self.entered.get_mut(&thread) else { return };
        let Some(at) = stack.iter().rposition(|it| it.id == id) else { return };
        let e = stack.remove(at);
        let total_cpu = match (usage.cpu_ns, e.usage.cpu_ns) {
            (Some(now), Some(then)) => Some((now - then).max(0)),
            _ => None,
        };
        if let (Some(cpu), Some(parent)) =
            (total_cpu, at.checked_sub(1).and_then(|i| stack.get_mut(i)))
        {
            parent.child_cpu += cpu;
        }
        let delta = |now: Option<u64>, then: Option<u64>| Some(now?.saturating_sub(then?));
        let Some(open) = self.open.get_mut(&id) else { return };
        open.entered = open.entered.saturating_sub(1);
        let s = &mut open.span;
        s.busy_ns = Some(s.busy_ns.unwrap_or(0) + (at_ns - e.at_ns).max(0));
        if let Some(cpu) = total_cpu {
            s.self_cpu_ns = Some(s.self_cpu_ns.unwrap_or(0) + (cpu - e.child_cpu).max(0));
        }
        if let Some(b) = delta(usage.alloc_bytes, e.usage.alloc_bytes) {
            s.alloc_bytes = Some(s.alloc_bytes.unwrap_or(0) + b);
        }
        if let Some(n) = delta(usage.allocs, e.usage.allocs) {
            s.allocs = Some(s.allocs.unwrap_or(0) + n);
        }
        if let Some(f) = delta(usage.freed_bytes, e.usage.freed_bytes) {
            s.frees_bytes = Some(s.frees_bytes.unwrap_or(0) + f);
        }
    }

    pub fn span_close(&mut self, id: u64, at_ns: i64) {
        self.advance_clock(at_ns);
        let Some(open) = self.open.remove(&id) else { return };
        let mut span = open.span;
        span.dur_ns = (at_ns - span.start_ns).max(0);
        if self.spans.push(span) {
            self.drop_records("spans", 1, "recorder ring full");
        }
    }

    /// Every open span, cut at now, by thread then start: what each thread is inside (and
    /// waiting on) right now.
    pub fn open_spans(&self) -> Vec<Span> {
        let mut all: Vec<Span> = self
            .open
            .values()
            .map(|o| {
                let mut s = o.span.clone();
                s.dur_ns = (self.now_ns - s.start_ns).max(0);
                s
            })
            .collect();
        all.sort_by(|a, b| {
            (a.thread, a.start_ns, a.span_id).cmp(&(b.thread, b.start_ns, b.span_id))
        });
        all
    }

    /// The innermost open span of `thread` it is inside now, for a sample's `span_id`.
    pub fn innermost(&self, thread: u64) -> Option<u64> {
        self.entered.get(&thread).and_then(|it| it.last()).map(|it| it.id)
    }

    pub fn cpu_sample(&mut self, sample: RawSample) {
        self.advance_clock(sample.t_ns);
        self.thread(sample.thread, None);
        if self.samples.push(sample) {
            self.drop_records("cpu_samples", 1, "recorder ring full");
        }
    }

    pub fn alloc(&mut self, a: RawAlloc) {
        self.advance_clock(a.t_ns);
        let span_name = a.span_id.and_then(|id| self.open.get(&id)).map(|it| it.span.name.clone());
        let mut key: AllocKey = (a.pcs.clone(), span_name);
        if !self.allocs.contains_key(&key) && self.allocs.len() >= self.limits.alloc_stacks {
            self.drop_records(
                "allocs",
                1,
                "too many distinct allocation stacks: folded into one row",
            );
            key = (Vec::new(), Some("<other stacks>".to_owned()));
        }
        let agg = self.allocs.entry(key.clone()).or_default();
        agg.alloc_count += a.count;
        agg.alloc_bytes += a.bytes;
        agg.live_count += a.count;
        agg.live_bytes += a.bytes;
        agg.sampled += 1;
        if self.live_blocks.len() < self.limits.live_blocks {
            self.live_blocks.insert(a.address, (key, a.bytes, a.count));
        } else {
            self.drop_records(
                "allocs",
                1,
                "too many sampled live blocks: its free will not be seen",
            );
        }
        if self.alloc_events.push(a) {
            self.drop_records("alloc_events", 1, "recorder ring full");
        }
    }

    /// A block was freed: if it was sampled, it is no longer live.
    pub fn free(&mut self, address: u64, t_ns: i64, thread: u64) {
        self.advance_clock(t_ns);
        let Some((key, bytes, count)) = self.live_blocks.remove(&address) else { return };
        if let Some(agg) = self.allocs.get_mut(&key) {
            agg.live_bytes = agg.live_bytes.saturating_sub(bytes);
            agg.live_count = agg.live_count.saturating_sub(count);
        }
        let free = RawAlloc { thread, t_ns, address, bytes, count, pcs: key.0, span_id: None };
        if self.frees.push(free) {
            self.drop_records("alloc_events", 1, "recorder ring full (frees)");
        }
    }

    pub fn counter(&mut self, name: &str, t_ns: i64, value: f64) {
        self.advance_clock(t_ns);
        let c = Counter { process: self.process, name: name.to_owned(), t_ns, value };
        if self.counters.push(c) {
            self.drop_records("counters", 1, "recorder ring full");
        }
    }

    /// The oldest instant every ring still covers.
    pub fn retained_from_ns(&self) -> i64 {
        let firsts = [
            self.spans.items.front().filter(|_| self.spans.base > 0).map(|it| it.start_ns),
            self.samples.items.front().filter(|_| self.samples.base > 0).map(|it| it.t_ns),
            self.alloc_events
                .items
                .front()
                .filter(|_| self.alloc_events.base > 0)
                .map(|it| it.t_ns),
        ];
        firsts.into_iter().flatten().max().unwrap_or(self.meta.start_ns)
    }

    /// Everything retained in `[since, until]` (all of it for `None`), as a canonical profile.
    /// Open spans are included, cut at `until` (their `dur_ns` so far).
    pub fn snapshot(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        symbols: &mut dyn Symbolize,
    ) -> Profile {
        let since = since.unwrap_or(i64::MIN);
        let until = until.unwrap_or(self.now_ns);
        let inside = |t: i64| t >= since && t <= until;
        let mut p = Profile { meta: self.meta.clone(), ..Profile::default() };
        p.meta.start_ns = since.max(self.meta.start_ns);
        p.meta.start_unix_ns = self.meta.start_unix_ns + (p.meta.start_ns - self.meta.start_ns);
        p.meta.end_ns = until;
        p.meta.symbolized = symbols.symbolized();
        let retained = self.retained_from_ns();
        if since < retained && retained > self.meta.start_ns {
            let missing: u64 = self.dropped.values().sum();
            p.meta.labels.insert(
                "window_complete".to_owned(),
                format!("false: {missing} records dropped or evicted before {retained}"),
            );
        } else {
            p.meta.labels.insert("window_complete".to_owned(), "true".to_owned());
        }
        p.threads = self.threads.values().cloned().collect();
        p.spans = self
            .spans
            .items
            .iter()
            .filter(|s| s.start_ns <= until && s.start_ns + s.dur_ns >= since)
            .cloned()
            .collect();
        for open in self.open.values().filter(|o| o.span.start_ns <= until) {
            let mut s = open.span.clone();
            s.dur_ns = (until - s.start_ns).max(0);
            p.spans.push(s);
        }
        let samples: Vec<&RawSample> =
            self.samples.items.iter().filter(|s| inside(s.t_ns)).collect();
        let events: Vec<&RawAlloc> =
            self.alloc_events.items.iter().filter(|a| inside(a.t_ns)).collect();
        let frees: Vec<&RawAlloc> = self.frees.items.iter().filter(|a| inside(a.t_ns)).collect();
        // Symbolize every address once.
        let mut addresses: Vec<u64> = samples
            .iter()
            .copied()
            .flat_map(|s: &RawSample| lookups(&s.pcs))
            .chain(events.iter().chain(&frees).copied().flat_map(|a: &RawAlloc| lookups(&a.pcs)))
            .chain(self.allocs.keys().flat_map(|(pcs, _)| lookups(pcs)))
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        let named = symbols.symbolize(&addresses);
        let mut interner = Interner::default();
        let chains: BTreeMap<u64, Vec<u32>> = addresses
            .iter()
            .zip(named)
            .map(|(a, frames)| {
                let mut frames = frames;
                if frames.is_empty() {
                    frames = NoSymbols.symbolize(&[*a]).remove(0);
                }
                (*a, frames.into_iter().map(|f| interner.frame(f)).collect())
            })
            .collect();
        let mut stack_of = |pcs: &[u64], span: Option<&str>, interner: &mut Interner| -> u32 {
            if pcs.is_empty() {
                return interner.named_stack(span.unwrap_or("<no stack>"), FrameKind::Alloc);
            }
            // Outermost first: the last return address first, each chain outermost first.
            let frames: Vec<u32> = lookups(pcs)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .flat_map(|a| chains.get(&a).cloned().unwrap_or_default())
                .collect();
            interner.stack(frames).unwrap_or(0)
        };
        for s in samples {
            let stack = stack_of(&s.pcs, None, &mut interner);
            p.cpu_samples.push(CpuSample {
                process: self.process,
                thread: s.thread,
                t_ns: s.t_ns,
                stack,
                span_id: s.span_id,
                on_cpu: s.on_cpu,
                weight_ns: s.weight_ns,
            });
        }
        for a in events {
            let stack = stack_of(&a.pcs, None, &mut interner);
            p.alloc_events.push(AllocEvent {
                process: self.process,
                thread: a.thread,
                t_ns: a.t_ns,
                stack,
                bytes: a.bytes as i64,
                span_id: a.span_id,
            });
        }
        for a in frees {
            let stack = stack_of(&a.pcs, None, &mut interner);
            p.alloc_events.push(AllocEvent {
                process: self.process,
                thread: a.thread,
                t_ns: a.t_ns,
                stack,
                bytes: -(a.bytes as i64),
                span_id: None,
            });
        }
        for ((pcs, span), agg) in &self.allocs {
            let stack = stack_of(pcs, span.as_deref(), &mut interner);
            p.allocs.push(Alloc {
                stack,
                span_name: span.clone(),
                alloc_count: agg.alloc_count,
                alloc_bytes: agg.alloc_bytes,
                live_count: agg.live_count,
                live_bytes: agg.live_bytes,
                sampled: agg.sampled,
            });
        }
        p.counters = self.counters.items.iter().filter(|c| inside(c.t_ns)).cloned().collect();
        p.frames = interner.frame_rows;
        p.stacks = interner.stack_rows;
        p.canonicalize();
        p
    }

    /// What changed since the last call: closed spans, samples, allocations, frees and
    /// counters, the open spans now, and the threads when one is new or renamed.
    pub fn take_live(&mut self) -> LiveDelta {
        let mut d = LiveDelta::default();
        if self.threads_sent != self.threads.len() {
            d.threads = self.threads.values().cloned().collect();
            self.threads_sent = self.threads.len();
        }
        macro_rules! take {
            ($ring:expr, $slot:expr, $name:expr, $into:expr) => {{
                let (items, missed) = $ring.since(self.live_from[$slot]);
                $into.extend(items.cloned());
                if missed > 0 {
                    d.missed.insert($name, missed);
                }
                self.live_from[$slot] = $ring.end();
            }};
        }
        take!(self.spans, 0, "spans", d.spans);
        take!(self.samples, 1, "cpu_samples", d.samples);
        take!(self.alloc_events, 2, "alloc_events", d.alloc_events);
        take!(self.frees, 3, "alloc_events", d.frees);
        take!(self.counters, 4, "counters", d.counters);
        let now = self.now_ns;
        d.open_spans = self
            .open
            .values()
            .map(|o| {
                let mut s = o.span.clone();
                s.dur_ns = (now - s.start_ns).max(0);
                s
            })
            .collect();
        d
    }
}

/// The addresses to look up for a stack: the interrupted PC as it is, each return address
/// less one, so it falls inside the call instruction and its line.
pub fn lookups(pcs: &[u64]) -> impl Iterator<Item = u64> + '_ {
    pcs.iter().enumerate().map(|(i, pc)| if i == 0 { *pc } else { pc.saturating_sub(1) })
}
