//! The profile as values: the tables of the `.ixprof` format (docs/profiling.md, "The file"),
//! one Rust struct a row. [`Profile::canonicalize`] puts it in the one order the format
//! writes, so the same recorded events give the same bytes.

use std::collections::BTreeMap;

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameKind {
    /// Machine code at an address.
    Native,
    /// A tracing span, as a frame of a span stack.
    Span,
    /// What a wait waits on, as the leaf of a wait stack.
    Wait,
    /// An allocation site with no native stack.
    Alloc,
}

impl FrameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Native => "native",
            FrameKind::Span => "span",
            FrameKind::Wait => "wait",
            FrameKind::Alloc => "alloc",
        }
    }

    pub fn parse(text: &str) -> Option<FrameKind> {
        Some(match text {
            "native" => FrameKind::Native,
            "span" => FrameKind::Span,
            "wait" => FrameKind::Wait,
            "alloc" => FrameKind::Alloc,
            _ => return None,
        })
    }
}

/// One code location at one inline level, or one span name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame {
    pub function: String,
    pub system_name: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub start_line: Option<u32>,
    pub module: Option<String>,
    pub build_id: Option<String>,
    /// Relative to the module's load base.
    pub address: Option<u64>,
    /// 0 for the function the machine code belongs to, 1 for the one inlined into it, ...
    pub inline_depth: u16,
    pub kind: FrameKind,
}

impl Frame {
    /// A frame named only by `function`, of `kind`.
    pub fn named(function: impl Into<String>, kind: FrameKind) -> Frame {
        Frame {
            function: function.into(),
            system_name: None,
            file: None,
            line: None,
            column: None,
            start_line: None,
            module: None,
            build_id: None,
            address: None,
            inline_depth: 0,
            kind,
        }
    }

    /// The order the format sorts frames in (docs: "Determinism").
    fn sort_key(&self) -> impl Ord + '_ {
        (
            &self.module,
            self.address,
            self.inline_depth,
            &self.function,
            &self.file,
            self.line,
            (&self.system_name, self.column, self.start_line, &self.build_id, self.kind),
        )
    }
}

/// A node of the stack trie: its caller's node and its own frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StackNode {
    pub parent: Option<u32>,
    pub frame: u32,
    pub depth: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub process: u32,
    pub process_name: String,
    pub thread: u64,
    pub thread_name: Option<String>,
    pub cpu_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuSample {
    pub process: u32,
    pub thread: u64,
    pub t_ns: i64,
    pub stack: u32,
    pub span_id: Option<u64>,
    pub on_cpu: bool,
    pub weight_ns: u64,
}

/// What a span row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpanKind {
    Span,
    Fn,
    Wait,
    Poll,
}

impl SpanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SpanKind::Span => "span",
            SpanKind::Fn => "fn",
            SpanKind::Wait => "wait",
            SpanKind::Poll => "poll",
        }
    }

    pub fn parse(text: &str) -> Option<SpanKind> {
        Some(match text {
            "span" => SpanKind::Span,
            "fn" => SpanKind::Fn,
            "wait" => SpanKind::Wait,
            "poll" => SpanKind::Poll,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub process: u32,
    pub thread: u64,
    pub span_id: u64,
    pub parent: Option<u64>,
    pub depth: u16,
    pub name: String,
    pub target: String,
    pub kind: SpanKind,
    pub task: Option<u64>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub start_ns: i64,
    pub dur_ns: i64,
    pub busy_ns: Option<i64>,
    pub self_cpu_ns: Option<i64>,
    pub alloc_bytes: Option<u64>,
    pub allocs: Option<u64>,
    pub frees_bytes: Option<u64>,
    pub wait: Option<String>,
    pub wait_on: Option<String>,
    pub wait_owner: Option<String>,
    /// `k=v` pairs sorted by key, joined by `\x1f`.
    pub fields: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alloc {
    pub stack: u32,
    pub span_name: Option<String>,
    pub alloc_count: u64,
    pub alloc_bytes: u64,
    pub live_count: u64,
    pub live_bytes: u64,
    pub sampled: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocEvent {
    pub process: u32,
    pub thread: u64,
    pub t_ns: i64,
    pub stack: u32,
    pub bytes: i64,
    pub span_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Counter {
    pub process: u32,
    pub name: String,
    pub t_ns: i64,
    pub value: f64,
}

/// The manifest's `meta`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Meta {
    pub program: String,
    pub producer: String,
    pub clock: String,
    pub start_ns: i64,
    pub end_ns: i64,
    pub start_unix_ns: i64,
    pub arch: String,
    pub os: String,
    pub cpu_period_ns: Option<u64>,
    pub cpu_sampler: String,
    pub alloc_sample_bytes: Option<u64>,
    pub alloc_counter: String,
    pub symbolized: bool,
    pub labels: BTreeMap<String, String>,
}

/// A whole profile.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Profile {
    pub meta: Meta,
    pub threads: Vec<Thread>,
    pub frames: Vec<Frame>,
    pub stacks: Vec<StackNode>,
    pub cpu_samples: Vec<CpuSample>,
    pub spans: Vec<Span>,
    pub allocs: Vec<Alloc>,
    pub alloc_events: Vec<AllocEvent>,
    pub counters: Vec<Counter>,
}

/// Builds a profile's frame and stack tables by interning.
#[derive(Default)]
pub struct Interner {
    frames: BTreeMap<Frame, u32>,
    stacks: BTreeMap<StackNode, u32>,
    pub frame_rows: Vec<Frame>,
    pub stack_rows: Vec<StackNode>,
}

impl Interner {
    pub fn frame(&mut self, frame: Frame) -> u32 {
        if let Some(&id) = self.frames.get(&frame) {
            return id;
        }
        let id = self.frame_rows.len() as u32;
        self.frame_rows.push(frame.clone());
        self.frames.insert(frame, id);
        id
    }

    /// The node for `frames`, outermost first; `None` for an empty stack.
    pub fn stack(&mut self, frames: impl IntoIterator<Item = u32>) -> Option<u32> {
        let mut parent: Option<u32> = None;
        let mut depth = 0u16;
        for frame in frames {
            let node = StackNode { parent, frame, depth };
            let id = match self.stacks.get(&node) {
                Some(&id) => id,
                None => {
                    let id = self.stack_rows.len() as u32;
                    self.stack_rows.push(node);
                    self.stacks.insert(node, id);
                    id
                }
            };
            parent = Some(id);
            depth = depth.saturating_add(1);
        }
        parent
    }

    /// The stack of a lone frame named `function`.
    pub fn named_stack(&mut self, function: &str, kind: FrameKind) -> u32 {
        let frame = self.frame(Frame::named(function, kind));
        self.stack([frame]).unwrap_or(0)
    }
}

impl Profile {
    /// The frames of `stack`, innermost first.
    pub fn frames_of(&self, stack: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = Some(stack);
        while let Some(node) = at.and_then(|it| self.stacks.get(it as usize)) {
            out.push(node.frame);
            at = node.parent;
        }
        out
    }

    /// Puts every table in the format's order and renumbers frames and stacks so their ids
    /// follow from content alone (docs: "Determinism"). Identical stacks merge.
    pub fn canonicalize(&mut self) {
        // Frames: sorted, deduplicated.
        let mut order: Vec<u32> = (0..self.frames.len() as u32).collect();
        order.sort_by(|a, b| {
            self.frames[*a as usize].sort_key().cmp(&self.frames[*b as usize].sort_key())
        });
        let mut frame_map = vec![0u32; self.frames.len()];
        let mut frames: Vec<Frame> = Vec::with_capacity(self.frames.len());
        for old in order {
            let frame = &self.frames[old as usize];
            if frames.last() != Some(frame) {
                frames.push(frame.clone());
            }
            frame_map[old as usize] = frames.len() as u32 - 1;
        }
        // Stacks: by path of new frame ids, root first.
        let paths: Vec<Vec<u32>> = (0..self.stacks.len() as u32)
            .map(|id| {
                let mut path: Vec<u32> =
                    self.frames_of(id).into_iter().map(|it| frame_map[it as usize]).collect();
                path.reverse();
                path
            })
            .collect();
        let mut sorted: Vec<&Vec<u32>> = paths.iter().collect();
        sorted.sort();
        sorted.dedup();
        let index: BTreeMap<&[u32], u32> =
            sorted.iter().enumerate().map(|(i, it)| (it.as_slice(), i as u32)).collect();
        let stacks: Vec<StackNode> = sorted
            .iter()
            .map(|path| {
                let (last, outer) = path.split_last().expect("a stack node has a frame");
                StackNode {
                    parent: (!outer.is_empty()).then(|| index[outer]),
                    frame: *last,
                    depth: outer.len() as u16,
                }
            })
            .collect();
        let stack_map: Vec<u32> = paths.iter().map(|it| index[it.as_slice()]).collect();
        let remap = |id: &mut u32| *id = stack_map.get(*id as usize).copied().unwrap_or(*id);
        for it in &mut self.cpu_samples {
            remap(&mut it.stack);
        }
        for it in &mut self.allocs {
            remap(&mut it.stack);
        }
        for it in &mut self.alloc_events {
            remap(&mut it.stack);
        }
        self.frames = frames;
        self.stacks = stacks;

        // Allocs of one (stack, span) merge.
        self.allocs.sort_by(|a, b| (a.stack, &a.span_name).cmp(&(b.stack, &b.span_name)));
        let mut allocs: Vec<Alloc> = Vec::with_capacity(self.allocs.len());
        for it in self.allocs.drain(..) {
            match allocs.last_mut() {
                Some(last) if last.stack == it.stack && last.span_name == it.span_name => {
                    last.alloc_count += it.alloc_count;
                    last.alloc_bytes += it.alloc_bytes;
                    last.live_count += it.live_count;
                    last.live_bytes += it.live_bytes;
                    last.sampled += it.sampled;
                }
                _ => allocs.push(it),
            }
        }
        self.allocs = allocs;

        self.threads.sort_by_key(|it| (it.process, it.thread));
        self.threads.dedup_by_key(|it| (it.process, it.thread));
        self.cpu_samples.sort_by_key(|it| (it.process, it.thread, it.t_ns, it.stack));
        self.spans.sort_by_key(|it| (it.process, it.start_ns, it.span_id));
        self.alloc_events.sort_by_key(|it| (it.process, it.thread, it.t_ns, it.stack, it.bytes));
        self.counters.sort_by(|a, b| {
            (&a.name, a.process, a.t_ns)
                .cmp(&(&b.name, b.process, b.t_ns))
                .then(a.value.total_cmp(&b.value))
        });
    }
}

/// `fields` as the format writes them: `k=v` sorted by key, `\x1f` between pairs.
pub fn join_fields(fields: &mut [(String, String)]) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    fields.sort();
    let mut out = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\x1f');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    Some(out)
}
