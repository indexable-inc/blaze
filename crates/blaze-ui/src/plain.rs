//! The rendering for what is not a terminal: lines, each said once.

use blaze_protocol::events::Record;

use crate::{Build, Change, GREEN, HEARTBEAT_NS, NEWS_NS, paint, staying, took};

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
    /// failed build has no last line of its own ([`crate::last_line`]).
    pub fn errors_said(self) -> Plain {
        Plain { errors_said: true, ..self }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    /// The lines `record` adds, each with its end. A step is said begun once it has gone on
    /// for [`NEWS_NS`]: one done sooner says nothing, as a log of every quick step is noise.
    pub fn record(&mut self, record: &Record) -> String {
        let mut said = self.begun_by(record.at_ns);
        let change = self.build.apply(record);
        let lines = match change {
            Change::Began(_) => Vec::new(),
            change => staying(&self.build, &change, self.color, false, self.errors_said),
        };
        if !lines.is_empty() {
            self.said_ns = record.at_ns;
        }
        said.extend(lines.into_iter().map(|it| it + "\n"));
        said
    }

    /// The steps under way for [`NEWS_NS`] by `now_ns` and not said begun yet, said begun.
    fn begun_by(&mut self, now_ns: u64) -> String {
        let mut said = String::new();
        while let Some(stepped) = self.build.steps.get(self.begun) {
            let ended = stepped.ended_ns.is_some();
            if !ended && now_ns.saturating_sub(stepped.began_ns) < NEWS_NS {
                break;
            }
            if !ended {
                let word = paint(GREEN, &format!("{:>12}", stepped.step.word()), self.color);
                said.push_str(&format!("{word} {}\n", stepped.what()));
                self.said_ns = now_ns;
            }
            self.begun += 1;
        }
        said
    }

    /// A line saying the step under way still is, when nothing was said for
    /// [`HEARTBEAT_NS`]: a log that stops for a minute looks like a hung build.
    pub fn tick(&mut self, now_ns: u64) -> String {
        if self.build.finished.is_some() {
            return String::new();
        }
        let begun = self.begun_by(now_ns);
        if !begun.is_empty() || now_ns.saturating_sub(self.said_ns) < HEARTBEAT_NS {
            return begun;
        }
        let Some(step) = self.build.running().last() else { return String::new() };
        self.said_ns = now_ns;
        let word = paint(GREEN, &format!("{:>12}", step.step.word()), self.color);
        let count = step.count().map(|it| format!(": {it}")).unwrap_or_default();
        let took = took(now_ns.saturating_sub(step.began_ns));
        format!("{word} {}{count} ({took})\n", step.what())
    }
}
