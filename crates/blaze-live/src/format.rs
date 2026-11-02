//! What the daemon and a running program say to each other outside a patch: the tiers, which
//! the program asks the daemon for ([`crate::tiers`]): which functions are hot and at which
//! level, and what it did of late ([`Tiers`]), for the program to show.

/// How many counters [`counters`](crate::blaze_live_counters) has room for.
pub const COUNTERS: usize = 1 << 16;

/// The levels a function is compiled at, from the quickest to build to the quickest to run.
pub const LEVELS: [&str; 3] = ["Cranelift", "LLVM -O3", "LLVM -O3 + PGO"];

/// What `level` (1, 2 or 3) is called.
pub fn level_name(level: u8) -> &'static str {
    LEVELS.get(usize::from(level).wrapping_sub(1)).copied().unwrap_or("?")
}

/// What the daemon says of the tiers of a running program.
#[derive(Debug, PartialEq, Default)]
pub struct Tiers {
    /// When it was written, in milliseconds since the epoch.
    pub at: u64,
    /// How long the counts of `functions` are over, in milliseconds.
    pub window_ms: u64,
    /// The samples taken in that window.
    pub samples: u64,
    /// The CPU the sampler took, in thousandths of the time it ran.
    pub sampler_permille: u64,
    /// The functions the samples fell in, the hottest first.
    pub functions: Vec<TierFunction>,
    /// What the daemon did, the newest last.
    pub events: Vec<TierEvent>,
}

#[derive(Debug, PartialEq, Default)]
pub struct TierFunction {
    /// 1, 2 or 3 ([`level_name`]); 0 for code the daemon did not compile and can't tier.
    pub level: u8,
    /// Samples in the window, and their share of all of them in thousandths.
    pub samples: u64,
    pub share: u64,
    pub name: String,
    /// What is under way or why it stays where it is, or nothing.
    pub note: String,
}

// clone: the daemon copies its recent events into each `Tiers` it writes, a few times a second
#[derive(Debug, PartialEq, Clone, Default)]
pub struct TierEvent {
    /// In milliseconds since the epoch.
    pub at: u64,
    pub text: String,
}

impl Tiers {
    /// Its text, written by the daemon a few times a second, every name and note on one line:
    /// a tab or a newline in one is written as a space.
    pub fn encode(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(
            96 + (self.functions.iter().map(|f| 48 + f.name.len() + f.note.len()))
                .chain(self.events.iter().map(|event| 32 + event.text.len()))
                .sum::<usize>(),
        );
        _ = write!(
            out,
            "at {}\nwindow_ms {}\nsamples {}\nsampler_permille {}\n",
            self.at, self.window_ms, self.samples, self.sampler_permille
        );
        for f in &self.functions {
            _ = write!(out, "fn\t{}\t{}\t{}\t", f.level, f.samples, f.share);
            push_clean(&mut out, &f.name);
            out.push('\t');
            push_clean(&mut out, &f.note);
            out.push('\n');
        }
        for event in &self.events {
            _ = write!(out, "event\t{}\t", event.at);
            push_clean(&mut out, &event.text);
            out.push('\n');
        }
        out
    }

    /// What [`Tiers::encode`] wrote, owned: the program's copy, asked of the daemon a few
    /// times a second ([`crate::tiers`]), not per frame.
    pub fn decode(text: &str) -> Option<Tiers> {
        let mut tiers = Tiers::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("fn\t") {
                let mut fields = rest.split('\t');
                let mut next = || fields.next();
                tiers.functions.push(TierFunction {
                    level: next()?.parse().ok()?,
                    samples: next()?.parse().ok()?,
                    share: next()?.parse().ok()?,
                    // alloc: the program's copy (see above)
                    name: next()?.to_owned(),
                    // alloc: the program's copy (see above)
                    note: next().unwrap_or_default().to_owned(),
                });
                continue;
            }
            if let Some(rest) = line.strip_prefix("event\t") {
                let (at, text) = rest.split_once('\t')?;
                // alloc: the program's copy (see above)
                tiers.events.push(TierEvent { at: at.parse().ok()?, text: text.to_owned() });
                continue;
            }
            let (key, value) = line.split_once(' ')?;
            match key {
                "at" => tiers.at = value.parse().ok()?,
                "window_ms" => tiers.window_ms = value.parse().ok()?,
                "samples" => tiers.samples = value.parse().ok()?,
                "sampler_permille" => tiers.sampler_permille = value.parse().ok()?,
                _ => {}
            }
        }
        (tiers.at != 0).then_some(tiers)
    }
}

/// `text` appended to `out` with each tab and newline a space.
fn push_clean(out: &mut String, text: &str) {
    out.extend(text.chars().map(|c| if c == '\t' || c == '\n' { ' ' } else { c }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiering_round_trip() {
        let tiers = Tiers {
            at: 10,
            window_ms: 1000,
            samples: 400,
            sampler_permille: 3,
            functions: vec![TierFunction {
                level: 2,
                samples: 300,
                share: 750,
                name: "<life::Life as Step>::step".into(),
                note: "profiling".into(),
            }],
            events: vec![TierEvent { at: 9, text: "step: LLVM -O3 in 120 ms".into() }],
        };
        assert_eq!(Tiers::decode(&tiers.encode()), Some(tiers));
        assert_eq!(level_name(3), "LLVM -O3 + PGO");
    }

    #[test]
    fn tiers_write_tabs_and_newlines_in_text_as_spaces() {
        let tiers = Tiers {
            at: 1,
            functions: vec![TierFunction {
                level: 1,
                samples: 2,
                share: 3,
                name: "a\tb\nc".into(),
                note: "\t".into(),
            }],
            events: vec![TierEvent { at: 4, text: "x\ny".into() }],
            ..Tiers::default()
        };
        let text = tiers.encode();
        assert_eq!(
            text,
            "at 1\nwindow_ms 0\nsamples 0\nsampler_permille 0\nfn\t1\t2\t3\ta b c\t \n\
             event\t4\tx y\n"
        );
        let read = Tiers::decode(&text).expect("tiers");
        assert_eq!((&read.functions[0].name[..], &read.events[0].text[..]), ("a b c", "x y"));
    }
}
