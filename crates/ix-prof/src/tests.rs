//! Two properties: what the recorder aggregates survives the .ixprof round trip byte for byte
//! and adds up in the views; a view survives the pprof round trip.

use hegel::{TestCase, generators as gs};

use crate::{
    core::{Limits, NewSpan, RawAlloc, RawSample, Recorder, Symbolize, Usage},
    profile::{Frame, FrameKind, Meta, Profile},
    report,
    view::{Kind, View},
};

/// Names an address `f<address>` in `src/f.rs` at line `address`, and every fourth address
/// gets an inlined frame: deterministic, so two runs symbolize alike.
struct Names;

impl Symbolize for Names {
    fn symbolize(&mut self, addresses: &[u64]) -> Vec<Vec<Frame>> {
        addresses
            .iter()
            .map(|a| {
                let mut f = Frame::named(format!("f{a}"), FrameKind::Native);
                f.address = Some(*a);
                f.module = Some("prog".to_owned());
                f.file = Some("src/f.rs".to_owned());
                f.line = Some(*a as u32 % 100 + 1);
                let mut out = vec![f];
                if a % 4 == 0 {
                    let mut g = Frame::named(format!("inlined{a}"), FrameKind::Native);
                    g.address = Some(*a);
                    g.module = Some("prog".to_owned());
                    g.inline_depth = 1;
                    out.push(g);
                }
                out
            })
            .collect()
    }

    fn symbolized(&self) -> bool {
        true
    }
}

fn meta() -> Meta {
    Meta { program: "prop".to_owned(), clock: "test".to_owned(), start_ns: 0, ..Meta::default() }
}

fn pcs(tc: &TestCase) -> Vec<u64> {
    tc.draw(gs::vecs(gs::integers::<u64>().min_value(1).max_value(40)).min_size(1).max_size(8))
}

/// A recorder driven by a drawn run of span opens, closes, samples, allocations and frees.
fn recorded(tc: &TestCase) -> (Recorder, u64) {
    let mut r = Recorder::new(7, "prop", meta(), Limits::default());
    let mut t = 0i64;
    let mut open: Vec<u64> = Vec::new();
    let mut next = 1u64;
    let mut samples = 0u64;
    let mut addresses: Vec<u64> = Vec::new();
    for _ in 0..tc.draw(gs::integers::<u32>().max_value(60)) {
        t += i64::from(tc.draw(gs::integers::<u32>().max_value(1000)));
        let thread = 1 + u64::from(tc.draw(gs::integers::<u8>().max_value(2)));
        let usage = Usage { cpu_ns: Some(t), ..Usage::default() };
        match tc.draw(gs::integers::<u8>().max_value(5)) {
            0 => {
                let id = next;
                next += 1;
                let new = NewSpan {
                    id,
                    parent: open.last().copied(),
                    thread,
                    name: format!("s{}", id % 5),
                    target: "prop".to_owned(),
                    wait: tc.draw(gs::booleans()).then(|| "lock".to_owned()),
                    ..NewSpan::default()
                };
                r.span_new(new, t);
                r.span_enter(id, thread, t, usage);
                open.push(id);
            }
            1 => {
                if let Some(id) = open.pop() {
                    r.span_exit(id, thread, t, usage);
                    r.span_close(id, t);
                }
            }
            2 | 3 => {
                samples += 1;
                let span_id = r.innermost(thread);
                r.cpu_sample(RawSample { thread, t_ns: t, pcs: pcs(tc), span_id, on_cpu: true, weight_ns: 1000 });
            }
            4 => {
                let address = 0x1000 + next * 64;
                next += 1;
                addresses.push(address);
                r.alloc(RawAlloc { thread, t_ns: t, address, bytes: 64, count: 1, pcs: pcs(tc), span_id: None });
            }
            _ => {
                if let Some(a) = addresses.pop() {
                    r.free(a, t, thread);
                }
            }
        }
    }
    r.advance_clock(t + 1);
    (r, samples)
}

#[hegel::test]
fn an_ixprof_reads_back_as_the_profile_written_and_writes_the_same_bytes(tc: TestCase) {
    let (r, samples) = recorded(&tc);
    let p = r.snapshot(None, None, &mut Names);
    let bytes = p.to_ixprof().expect("writes");
    assert_eq!(bytes, p.to_ixprof().expect("writes again"), "the same profile, the same bytes");
    let back = Profile::from_ixprof(&bytes).expect("reads");
    let mut want = p.clone();
    want.canonicalize();
    assert_eq!(back, want);
    assert_eq!(back.to_ixprof().expect("rewrites"), bytes, "read and written again, the same bytes");
    // Aggregation: every sample is counted once, and top's flat column adds up to the total.
    let cpu = View::of(&back, Kind::Cpu);
    let count = cpu.type_index(Some("samples")).expect("a samples type");
    assert_eq!(report::total(&cpu, count), samples as i64);
    assert_eq!(back.cpu_samples.len() as u64, samples);
}

fn frame(tc: &TestCase, i: usize) -> Frame {
    let kind = if tc.draw(gs::booleans()) { FrameKind::Native } else { FrameKind::Span };
    let mut f = Frame::named(format!("fn_{i}"), kind);
    if kind == FrameKind::Native {
        f.address = Some(0x100 + i as u64 * 16);
        f.module = Some("prog".to_owned());
        f.file = tc.draw(gs::booleans()).then(|| format!("src/m{}.rs", i % 3));
        f.line = f.file.as_ref().map(|_| 1 + i as u32);
    }
    f
}

#[hegel::test]
fn a_view_reads_back_from_pprof_as_written(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let frames: Vec<Frame> = (0..n).map(|i| frame(&tc, i)).collect();
    let samples = tc.draw(gs::integers::<usize>().max_value(20));
    let mut view = View {
        sample_types: vec![
            crate::view::ValueType::new("samples", "count"),
            crate::view::ValueType::new("cpu", "nanoseconds"),
        ],
        default_type: 1,
        period: Some((crate::view::ValueType::new("cpu", "nanoseconds"), 1_000_000)),
        frames,
        samples: Vec::new(),
        time_nanos: 5,
        duration_nanos: 1_000,
    };
    for _ in 0..samples {
        let stack: Vec<u32> = tc.draw(gs::vecs(gs::integers::<u32>().max_value(n as u32 - 1)).min_size(1).max_size(6));
        let v = i64::from(tc.draw(gs::integers::<u32>().max_value(1 << 20)));
        let labels = if tc.draw(gs::booleans()) {
            vec![("thread".to_owned(), crate::view::LabelValue::Str("main".to_owned()))]
        } else {
            Vec::new()
        };
        view.samples.push(crate::view::Sample { stack, values: vec![1, v], labels });
    }
    view.canonicalize();
    let back = View::from_pprof(&view.to_pprof()).expect("reads");
    assert_eq!(back, view);
    assert_eq!(back.to_pprof(), view.to_pprof(), "the same view, the same bytes");
}
