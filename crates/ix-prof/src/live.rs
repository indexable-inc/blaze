//! The live stream (docs/profiling.md, "Live: the streaming endpoint"), sans-IO: the
//! [`Encoder`] turns each tick's [`LiveDelta`] into frames with session-local, append-only
//! frame and stack ids; the [`Log`] keeps them for resume within a byte budget and answers
//! "what follows this cursor", with a GAP naming what is gone. The HTTP driver only writes
//! the bytes.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use crate::{
    core::{LiveDelta, RawAlloc, Symbolize, lookups},
    ixprof::{self, Error, Table},
    json::{self, Json},
    profile::{CpuSample, Frame, FrameKind, Meta, StackNode},
};

pub const HELLO: u8 = 1;
pub const BATCH: u8 = 2;
pub const TICK: u8 = 3;
pub const GAP: u8 = 4;
pub const END: u8 = 5;

/// One frame: `len:u32 LE | kind:u8 | payload`, `len` counting the kind and the payload.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// A BATCH frame of `table` at `seq`.
pub fn batch(seq: u64, table: &Table) -> Result<Vec<u8>, Error> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&seq.to_le_bytes());
    payload.push(table.name.len() as u8);
    payload.extend_from_slice(table.name.as_bytes());
    payload.extend_from_slice(&table.ipc_stream()?);
    Ok(frame(BATCH, &payload))
}

/// A frame as a reader sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum Read<'a> {
    Hello(&'a str),
    Batch { seq: u64, table: &'a str, ipc: &'a [u8] },
    Tick(&'a str),
    Gap(&'a str),
    End(&'a str),
}

/// The first frame of `bytes` and the bytes after it; `None` when a whole frame is not there yet.
pub fn read(bytes: &[u8]) -> Result<Option<(Read<'_>, &[u8])>, String> {
    let Some(len) = bytes.get(..4) else { return Ok(None) };
    let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
    if len == 0 {
        return Err("a frame of length 0 has no kind".to_owned());
    }
    let Some(body) = bytes.get(4..4 + len) else { return Ok(None) };
    let rest = &bytes[4 + len..];
    let (kind, payload) = (body[0], &body[1..]);
    let text = || std::str::from_utf8(payload).map_err(|_| format!("frame kind {kind}: not UTF-8"));
    let frame = match kind {
        HELLO => Read::Hello(text()?),
        TICK => Read::Tick(text()?),
        GAP => Read::Gap(text()?),
        END => Read::End(text()?),
        BATCH => {
            let seq = u64::from_le_bytes(
                payload
                    .get(..8)
                    .ok_or("a BATCH frame shorter than its seq")?
                    .try_into()
                    .map_err(|_| "seq")?,
            );
            let n = *payload.get(8).ok_or("a BATCH frame with no table name")? as usize;
            let name = payload.get(9..9 + n).ok_or("a BATCH frame's table name runs past it")?;
            let table =
                std::str::from_utf8(name).map_err(|_| "a BATCH frame's table name is not UTF-8")?;
            Read::Batch { seq, table, ipc: &payload[9 + n..] }
        }
        other => return Err(format!("unknown frame kind {other}")),
    };
    Ok(Some((frame, rest)))
}

/// A session's cursor: `<session>.<seq hex>`.
pub fn cursor(session: &str, seq: u64) -> String {
    format!("{session}.{seq:x}")
}

pub fn parse_cursor(text: &str) -> Option<(&str, u64)> {
    let (session, seq) = text.rsplit_once('.')?;
    Some((session, u64::from_str_radix(seq, 16).ok()?))
}

/// The session id: blake3 of (pid, start_ns, start_unix_ns, nonce), first 16 hex digits.
/// The nonce is random, so two runs with the same pid (pid reuse, or the first process of
/// two containers) and the same clock readings still get different ids, and different
/// profile file names.
pub fn session_id(pid: u32, start_ns: i64, start_unix_ns: i64, nonce: u64) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&pid.to_le_bytes());
    h.update(&start_ns.to_le_bytes());
    h.update(&start_unix_ns.to_le_bytes());
    h.update(&nonce.to_le_bytes());
    h.finalize().to_hex()[..16].to_owned()
}

/// Turns ticks into frames, keeping the catalog (frames, stacks) append-only.
pub struct Encoder {
    pub session: String,
    pub pid: u32,
    seq: u64,
    frames: BTreeMap<Frame, u32>,
    frame_rows: Vec<Frame>,
    stacks: BTreeMap<StackNode, u32>,
    stack_rows: Vec<StackNode>,
    chains: BTreeMap<u64, Vec<u32>>,
}

/// One tick's frames, each with its seq and whether it is catalog.
pub struct Tick {
    pub frames: Vec<(u64, &'static str, bool, Vec<u8>)>,
}

impl Encoder {
    pub fn new(session: String, pid: u32) -> Encoder {
        Encoder {
            session,
            pid,
            seq: 0,
            frames: BTreeMap::new(),
            frame_rows: Vec::new(),
            stacks: BTreeMap::new(),
            stack_rows: Vec::new(),
            chains: BTreeMap::new(),
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn frame_id(&mut self, f: Frame) -> u32 {
        if let Some(&id) = self.frames.get(&f) {
            return id;
        }
        let id = self.frame_rows.len() as u32;
        self.frame_rows.push(f.clone());
        self.frames.insert(f, id);
        id
    }

    fn stack_id(&mut self, pcs: &[u64]) -> u32 {
        let mut parent = None;
        let mut depth = 0u16;
        let addrs: Vec<u64> = lookups(pcs).collect();
        for a in addrs.into_iter().rev() {
            for frame in self.chains.get(&a).cloned().unwrap_or_default() {
                let node = StackNode { parent, frame, depth };
                let id = match self.stacks.get(&node) {
                    Some(id) => *id,
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
        }
        match parent {
            Some(id) => id,
            None => {
                let frame = self.frame_id(Frame::named("<no stack>", FrameKind::Alloc));
                let node = StackNode { parent: None, frame, depth: 0 };
                *self.stacks.entry(node).or_insert_with(|| {
                    self.stack_rows.push(node);
                    self.stack_rows.len() as u32 - 1
                })
            }
        }
    }

    /// The frames of one tick of `delta`.
    pub fn tick(&mut self, delta: &LiveDelta, symbols: &mut dyn Symbolize) -> Result<Tick, Error> {
        let frames_before = self.frame_rows.len();
        let stacks_before = self.stack_rows.len();
        // Name each new address once.
        let mut new: Vec<u64> = delta
            .samples
            .iter()
            .flat_map(|s| lookups(&s.pcs))
            .chain(
                delta
                    .alloc_events
                    .iter()
                    .chain(&delta.frees)
                    .flat_map(|a: &RawAlloc| lookups(&a.pcs)),
            )
            .filter(|a| !self.chains.contains_key(a))
            .collect();
        new.sort_unstable();
        new.dedup();
        for (a, chain) in new.iter().zip(symbols.symbolize(&new)) {
            let ids = chain.into_iter().map(|f| self.frame_id(f)).collect();
            self.chains.insert(*a, ids);
        }
        let samples: Vec<CpuSample> = delta
            .samples
            .iter()
            .map(|s| CpuSample {
                process: self.pid,
                thread: s.thread,
                t_ns: s.t_ns,
                stack: self.stack_id(&s.pcs),
                span_id: s.span_id,
                on_cpu: s.on_cpu,
                weight_ns: s.weight_ns,
            })
            .collect();
        let events: Vec<crate::profile::AllocEvent> = delta
            .alloc_events
            .iter()
            .map(|a| (a, 1i64))
            .chain(delta.frees.iter().map(|a| (a, -1i64)))
            .map(|(a, sign)| crate::profile::AllocEvent {
                process: self.pid,
                thread: a.thread,
                t_ns: a.t_ns,
                stack: self.stack_id(&a.pcs),
                bytes: sign * a.bytes as i64,
                span_id: a.span_id,
            })
            .collect();
        let mut tables: Vec<(&'static str, bool, Table)> = vec![
            ("threads", true, ixprof::threads(&delta.threads)),
            ("frames", true, ixprof::frames(&self.frame_rows[frames_before..])),
            ("stacks", true, ixprof::stacks(&self.stack_rows[stacks_before..])),
            ("spans", false, ixprof::spans("spans", &delta.spans)),
            ("cpu_samples", false, ixprof::cpu_samples(&samples)),
        ];
        let rest = crate::profile::Profile {
            alloc_events: events,
            counters: delta.counters.clone(),
            ..Default::default()
        };
        for t in ixprof::tables(&rest) {
            tables.push((t.name, false, t));
        }
        tables.push(("open_spans", false, ixprof::spans("open_spans", &delta.open_spans)));
        let mut out = Tick { frames: Vec::new() };
        for (name, catalog, table) in tables {
            if table.rows() == 0 && name != "open_spans" {
                continue;
            }
            self.seq += 1;
            out.frames.push((self.seq, name, catalog, batch(self.seq, &table)?));
        }
        Ok(out)
    }
}

/// What the stream keeps for resume: data frames within a byte budget, catalog frames whole.
pub struct Log {
    pub session: String,
    pub retain_bytes: usize,
    data: VecDeque<(u64, &'static str, Arc<[u8]>)>,
    data_bytes: usize,
    catalog: Vec<(u64, Arc<[u8]>)>,
    evicted: VecDeque<(u64, &'static str)>,
    last_seq: u64,
    last_tick: Option<Json>,
    pub ended: Option<String>,
}

/// What follows a cursor.
pub struct Since {
    /// GAP, when data frames after the cursor are gone.
    pub gap: Option<Vec<u8>>,
    pub frames: Vec<Arc<[u8]>>,
    pub next: u64,
}

impl Log {
    pub fn new(session: String, retain_bytes: usize) -> Log {
        Log {
            session,
            retain_bytes,
            data: VecDeque::new(),
            data_bytes: 0,
            catalog: Vec::new(),
            evicted: VecDeque::new(),
            last_seq: 0,
            last_tick: None,
            ended: None,
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Keeps one tick's frames and the TICK that follows them.
    pub fn push(
        &mut self,
        tick: Tick,
        now_ns: i64,
        dropped: &BTreeMap<&'static str, (u64, String)>,
        open_spans: usize,
    ) {
        for (seq, name, catalog, bytes) in tick.frames {
            self.last_seq = self.last_seq.max(seq);
            let bytes: Arc<[u8]> = bytes.into();
            if catalog {
                self.catalog.push((seq, bytes));
                continue;
            }
            self.data_bytes += bytes.len();
            self.data.push_back((seq, name, bytes));
            while self.data_bytes > self.retain_bytes && self.data.len() > 1 {
                let Some((seq, name, gone)) = self.data.pop_front() else { break };
                self.data_bytes -= gone.len();
                self.evicted.push_back((seq, name));
                if self.evicted.len() > 1 << 20 {
                    self.evicted.pop_front();
                }
            }
        }
        let evicted_before = self.data.front().map(|it| it.0).unwrap_or(self.last_seq + 1);
        self.last_tick = Some(json::obj([
            ("now_ns", Json::Int(i128::from(now_ns))),
            ("seq", Json::Int(i128::from(self.last_seq))),
            (
                "dropped",
                Json::Obj(
                    dropped
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), Json::Int(i128::from(v.0))))
                        .collect(),
                ),
            ),
            (
                "dropped_reason",
                Json::Obj(
                    dropped.iter().map(|(k, v)| ((*k).to_owned(), json::str(&v.1))).collect(),
                ),
            ),
            ("first_retained_seq", Json::Int(i128::from(evicted_before))),
            ("open_spans", Json::Int(open_spans as i128)),
        ]));
    }

    /// The HELLO frame for a client resuming from `cursor` (or starting fresh).
    pub fn hello(&self, meta: &Meta, cursor: Option<&str>) -> (Vec<u8>, u64) {
        let resumed = cursor.and_then(parse_cursor).filter(|(s, _)| *s == self.session);
        let from = resumed.map_or(0, |it| it.1);
        let mut meta_json = ixprof::meta_json(meta);
        if let Json::Obj(map) = &mut meta_json {
            map.remove("end_ns");
        }
        let hello = json::obj([
            ("session", json::str(&self.session)),
            ("meta", meta_json),
            ("cursor", json::str(&self::cursor(&self.session, from))),
            ("resumed", Json::Bool(resumed.is_some())),
            ("retain_bytes", Json::Int(self.retain_bytes as i128)),
        ]);
        (frame(HELLO, hello.to_text().as_bytes()), from)
    }

    /// Every frame after `from` (catalog frames always, data frames while retained), and a
    /// GAP naming what is gone. A fresh client (`from == 0`) gets the catalog and only data
    /// from now on, when `fresh_from_now`.
    pub fn since(&self, from: u64, fresh_from_now: bool) -> Since {
        let data_from = if from == 0 && fresh_from_now { self.last_seq } else { from };
        let mut frames: Vec<(u64, Arc<[u8]>)> = self
            .catalog
            .iter()
            .filter(|it| it.0 > from)
            .map(|(s, b)| (*s, Arc::clone(b)))
            .collect();
        frames.extend(
            self.data.iter().filter(|it| it.0 > data_from).map(|(s, _, b)| (*s, Arc::clone(b))),
        );
        frames.sort_by_key(|it| it.0);
        let first_retained = self.data.front().map_or(self.last_seq + 1, |it| it.0);
        let gap = (data_from + 1 < first_retained && !(from == 0 && fresh_from_now)).then(|| {
            let mut tables: BTreeMap<String, Json> = BTreeMap::new();
            let mut lost = 0u64;
            for (seq, name) in &self.evicted {
                if *seq > data_from && *seq < first_retained {
                    lost += 1;
                    let e = tables.entry((*name).to_owned()).or_insert(Json::Int(0));
                    if let Json::Int(n) = e {
                        *n += 1;
                    }
                }
            }
            let gap = json::obj([
                ("from_seq", Json::Int(i128::from(data_from + 1))),
                ("to_seq", Json::Int(i128::from(first_retained - 1))),
                ("tables", Json::Obj(tables)),
                ("batches", Json::Int(i128::from(lost))),
                (
                    "why",
                    json::str(&format!(
                        "cursor older than retention ({} bytes)",
                        self.retain_bytes
                    )),
                ),
            ]);
            frame(GAP, gap.to_text().as_bytes())
        });
        Since { gap, frames: frames.into_iter().map(|it| it.1).collect(), next: self.last_seq }
    }

    /// The TICK frame for a client that has everything up to `seq`.
    pub fn tick_frame(&self, seq: u64) -> Option<Vec<u8>> {
        let mut tick = self.last_tick.clone()?;
        if let Json::Obj(map) = &mut tick {
            map.insert("cursor".to_owned(), json::str(&cursor(&self.session, seq)));
        }
        Some(frame(TICK, tick.to_text().as_bytes()))
    }

    pub fn end_frame(why: &str) -> Vec<u8> {
        frame(END, json::obj([("why", json::str(why))]).to_text().as_bytes())
    }
}
