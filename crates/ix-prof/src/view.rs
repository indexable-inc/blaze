//! A weighted-stack view of a profile: what pprof calls a profile, and what every verb
//! (`top`, `peek`, `tree`, `list`, `flame`, `diff`) reads. Made from an `.ixprof` for one kind
//! (cpu, heap, spans, waits) or read from a pprof file.

use std::collections::BTreeMap;

use crate::profile::{Frame, FrameKind, Profile};

/// Which view of an `.ixprof` (docs: "pprof", the table of kinds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cpu,
    Heap,
    Spans,
    Waits,
}

impl Kind {
    pub fn parse(text: &str) -> Option<Kind> {
        Some(match text {
            "cpu" | "profile" => Kind::Cpu,
            "heap" | "allocs" => Kind::Heap,
            "spans" => Kind::Spans,
            "waits" => Kind::Waits,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ValueType {
    pub name: String,
    pub unit: String,
}

impl ValueType {
    pub fn new(name: &str, unit: &str) -> ValueType {
        ValueType { name: name.to_owned(), unit: unit.to_owned() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelValue {
    Str(String),
    Num(i64),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sample {
    /// Rows of [`View::frames`], innermost first (pprof's order).
    pub stack: Vec<u32>,
    pub values: Vec<i64>,
    pub labels: Vec<(String, LabelValue)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct View {
    pub sample_types: Vec<ValueType>,
    /// Index into `sample_types` the verbs show by default.
    pub default_type: usize,
    pub period: Option<(ValueType, i64)>,
    pub frames: Vec<Frame>,
    pub samples: Vec<Sample>,
    pub time_nanos: i64,
    pub duration_nanos: i64,
}

impl View {
    /// The view of `p` for `kind`.
    pub fn of(p: &Profile, kind: Kind) -> View {
        let mut v = View {
            time_nanos: p.meta.start_unix_ns,
            duration_nanos: p.meta.end_ns.saturating_sub(p.meta.start_ns),
            frames: p.frames.clone(),
            ..View::default()
        };
        let thread_names: BTreeMap<(u32, u64), &str> = p
            .threads
            .iter()
            .filter_map(|t| Some(((t.process, t.thread), t.thread_name.as_deref()?)))
            .collect();
        let spans_by_id: BTreeMap<(u32, u64), usize> =
            p.spans.iter().enumerate().map(|(i, s)| ((s.process, s.span_id), i)).collect();
        let thread_labels = |process: u32, thread: u64| {
            let mut labels = vec![("thread_id".to_owned(), LabelValue::Num(thread as i64))];
            if let Some(name) = thread_names.get(&(process, thread)) {
                labels.push(("thread".to_owned(), LabelValue::Str((*name).to_owned())));
            }
            labels
        };
        match kind {
            Kind::Cpu => {
                v.sample_types =
                    vec![ValueType::new("samples", "count"), ValueType::new("cpu", "nanoseconds")];
                v.default_type = 1;
                v.period = p
                    .meta
                    .cpu_period_ns
                    .map(|it| (ValueType::new("cpu", "nanoseconds"), it as i64));
                for s in &p.cpu_samples {
                    let mut labels = thread_labels(s.process, s.thread);
                    if let Some(span) = s.span_id.and_then(|id| spans_by_id.get(&(s.process, id))) {
                        labels.push((
                            "span".to_owned(),
                            LabelValue::Str(p.spans[*span].name.clone()),
                        ));
                    }
                    if !s.on_cpu {
                        labels.push(("cpu".to_owned(), LabelValue::Str("off".to_owned())));
                    }
                    v.samples.push(Sample {
                        stack: p.frames_of(s.stack),
                        values: vec![1, s.weight_ns as i64],
                        labels,
                    });
                }
            }
            Kind::Heap => {
                v.sample_types = vec![
                    ValueType::new("alloc_objects", "count"),
                    ValueType::new("alloc_space", "bytes"),
                    ValueType::new("inuse_objects", "count"),
                    ValueType::new("inuse_space", "bytes"),
                ];
                v.default_type = 3;
                v.period = p
                    .meta
                    .alloc_sample_bytes
                    .map(|it| (ValueType::new("space", "bytes"), it as i64));
                for a in &p.allocs {
                    let mut labels = Vec::new();
                    if let Some(span) = &a.span_name {
                        labels.push(("span".to_owned(), LabelValue::Str(span.clone())));
                    }
                    v.samples.push(Sample {
                        stack: p.frames_of(a.stack),
                        values: vec![
                            a.alloc_count as i64,
                            a.alloc_bytes as i64,
                            a.live_count as i64,
                            a.live_bytes as i64,
                        ],
                        labels,
                    });
                }
            }
            Kind::Spans | Kind::Waits => {
                v.sample_types = match kind {
                    Kind::Spans => vec![
                        ValueType::new("calls", "count"),
                        ValueType::new("wall", "nanoseconds"),
                        ValueType::new("self_cpu", "nanoseconds"),
                        ValueType::new("alloc_space", "bytes"),
                    ],
                    _ => vec![
                        ValueType::new("waits", "count"),
                        ValueType::new("wait", "nanoseconds"),
                    ],
                };
                v.default_type = 1;
                // Each span's frame: its name, at its callsite.
                let mut frame_of: BTreeMap<(String, Option<String>, Option<u32>, FrameKind), u32> =
                    BTreeMap::new();
                let mut frame = |frames: &mut Vec<Frame>,
                                 name: &str,
                                 file: Option<&String>,
                                 line: Option<u32>,
                                 kind: FrameKind| {
                    let key = (name.to_owned(), file.cloned(), line, kind);
                    *frame_of.entry(key).or_insert_with(|| {
                        let mut f = Frame::named(name, kind);
                        f.file = file.cloned();
                        f.line = line;
                        f.start_line = line;
                        frames.push(f);
                        frames.len() as u32 - 1
                    })
                };
                // Children's wall time, to give each span its own.
                let mut child_wall: BTreeMap<(u32, u64), i64> = BTreeMap::new();
                for s in &p.spans {
                    if let Some(parent) = s.parent {
                        *child_wall.entry((s.process, parent)).or_default() += s.dur_ns;
                    }
                }
                for s in &p.spans {
                    let is_wait = s.wait.is_some();
                    if kind == Kind::Waits && !is_wait {
                        continue;
                    }
                    let mut stack = Vec::new();
                    if kind == Kind::Waits {
                        let what = format!(
                            "wait {}: {}",
                            s.wait.as_deref().unwrap_or("other"),
                            s.wait_on.as_deref().unwrap_or("?")
                        );
                        stack.push(frame(&mut v.frames, &what, None, None, FrameKind::Wait));
                    }
                    let mut at = Some(s);
                    let mut hops = 0;
                    while let Some(span) = at {
                        stack.push(frame(
                            &mut v.frames,
                            &span.name,
                            span.file.as_ref(),
                            span.line,
                            FrameKind::Span,
                        ));
                        at = span
                            .parent
                            .and_then(|id| spans_by_id.get(&(span.process, id)))
                            .map(|i| &p.spans[*i]);
                        hops += 1;
                        if hops > 4096 {
                            break;
                        }
                    }
                    let mut labels = thread_labels(s.process, s.thread);
                    if let Some(owner) = &s.wait_owner {
                        labels.push(("owner".to_owned(), LabelValue::Str(owner.clone())));
                    }
                    let values = match kind {
                        Kind::Spans => {
                            let own = s.dur_ns
                                - child_wall.get(&(s.process, s.span_id)).copied().unwrap_or(0);
                            vec![
                                1,
                                own.max(0),
                                s.self_cpu_ns.unwrap_or(0),
                                s.alloc_bytes.unwrap_or(0) as i64,
                            ]
                        }
                        _ => vec![1, s.dur_ns],
                    };
                    v.samples.push(Sample { stack, values, labels });
                }
            }
        }
        v.canonicalize();
        v
    }

    /// Sorts frames and samples so the view's bytes follow from its content.
    pub fn canonicalize(&mut self) {
        let mut order: Vec<u32> = (0..self.frames.len() as u32).collect();
        order.sort_by(|a, b| self.frames[*a as usize].cmp(&self.frames[*b as usize]));
        let mut map = vec![0u32; self.frames.len()];
        let mut frames: Vec<Frame> = Vec::with_capacity(self.frames.len());
        for old in order {
            let f = &self.frames[old as usize];
            if frames.last() != Some(f) {
                frames.push(f.clone());
            }
            map[old as usize] = frames.len() as u32 - 1;
        }
        // Frames no sample reaches are dropped.
        let mut used = vec![false; frames.len()];
        for s in &mut self.samples {
            for id in &mut s.stack {
                *id = map[*id as usize];
                used[*id as usize] = true;
            }
            s.labels.sort();
        }
        let mut keep = vec![0u32; frames.len()];
        let mut kept = Vec::new();
        for (i, f) in frames.into_iter().enumerate() {
            if used[i] {
                keep[i] = kept.len() as u32;
                kept.push(f);
            }
        }
        for s in &mut self.samples {
            for id in &mut s.stack {
                *id = keep[*id as usize];
            }
        }
        self.frames = kept;
        self.samples.sort();
    }

    /// The function names of a sample's stack, outermost first.
    pub fn names(&self, sample: &Sample) -> Vec<&str> {
        sample.stack.iter().rev().map(|id| self.frames[*id as usize].function.as_str()).collect()
    }

    /// The index of sample type `name`, or the default.
    pub fn type_index(&self, name: Option<&str>) -> Option<usize> {
        match name {
            None => Some(self.default_type.min(self.sample_types.len().saturating_sub(1))),
            Some(name) => self.sample_types.iter().position(|it| it.name == name),
        }
    }
}
