//! The rendering for what is not a terminal: lines, each said once.

use blaze_protocol::events::Record;

use crate::{Build, Change, Count, Dur, HEARTBEAT_NS, NEWS_NS, staying, status};

pub struct Plain {
    build: Build,
    /// When a line was last written.
    said_ns: u64,
    /// How many of the build's steps were said begun, or were done before they were worth it.
    begun: usize,
    color: bool,
    errors_said: bool,
}

impl Plain {
    pub fn new(color: bool) -> Plain {
        Plain { build: Build::default(), said_ns: 0, begun: 0, color, errors_said: false }
    }

    /// For the command that asked for the build, which says its errors in full after it: a
    /// failed build has no last line of its own ([`crate::last_lines`]).
    pub fn errors_said(self) -> Plain {
        Plain { errors_said: true, ..self }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    /// The lines `record` adds, each with its end. A step is said begun once it has gone on
    /// for [`NEWS_NS`]: one done sooner says nothing, as a log of every quick step is noise.
    pub fn record(&mut self, record: &Record) -> String {
        // alloc: the one-call form for the CLI; a loop writes `record_into` one buffer
        let mut out = String::new();
        self.record_into(record, &mut out);
        out
    }

    /// [`Plain::record`], written at the end of `out`.
    pub fn record_into(&mut self, record: &Record, out: &mut String) {
        self.begun_by(record.at_ns, out);
        let change = self.build.apply(record);
        if let Change::Began(_) = change {
            return;
        }
        let from = out.len();
        staying(&self.build, &change, self.color, false, self.errors_said, out);
        if out.len() > from {
            self.said_ns = record.at_ns;
        }
    }

    /// Says begun the steps under way for [`NEWS_NS`] by `now_ns` and not said begun yet, and
    /// whether it said any.
    fn begun_by(&mut self, now_ns: u64, out: &mut String) -> bool {
        let from = out.len();
        while let Some(stepped) = self.build.steps.get(self.begun) {
            let ended = stepped.ended_ns.is_some();
            if !ended && now_ns.saturating_sub(stepped.began_ns) < NEWS_NS {
                break;
            }
            if !ended {
                status(out, stepped.step.word(), self.color);
                out.push_str(stepped.what());
                out.push('\n');
                self.said_ns = now_ns;
            }
            self.begun += 1;
        }
        out.len() > from
    }

    /// A line saying the step under way still is, when nothing was said for
    /// [`HEARTBEAT_NS`]: a log that stops for a minute looks like a hung build.
    pub fn tick(&mut self, now_ns: u64) -> String {
        // alloc: the one-call form for the CLI; a loop writes `tick_into` one buffer
        let mut out = String::new();
        self.tick_into(now_ns, &mut out);
        out
    }

    pub fn tick_into(&mut self, now_ns: u64, out: &mut String) {
        if self.build.finished.is_some() {
            return;
        }
        if self.begun_by(now_ns, out) || now_ns.saturating_sub(self.said_ns) < HEARTBEAT_NS {
            return;
        }
        let Some(step) = self.build.running().last() else { return };
        self.said_ns = now_ns;
        status(out, step.step.word(), self.color);
        out.push_str(step.what());
        if let Some(progress) = &step.progress {
            put!(out, ": {}", Count(progress, step.step));
        }
        put!(out, " ({})\n", Dur(now_ns.saturating_sub(step.began_ns)));
    }
}
