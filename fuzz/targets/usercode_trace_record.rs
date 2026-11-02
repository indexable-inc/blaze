//! A sandbox's live record of what a program (user code) read, which a sandboxed program can
//! write to on macOS: `blaze_usercode::trace::parse(text)`, read whole and as it grows (each
//! read taking up from where the last one's `used` ended, as a warm process's record is read
//! after each answer). The input's bytes are the text, made UTF-8 by `from_utf8_lossy`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - A line ended by `\n` is a record's line exactly when it is one of the letters
//!   `RLDETWNX`, a tab, a scope with no tab, a tab and the rest; the record is read when each
//!   is, else the error names the first that is not (from 1) and its first 200 characters.
//! - What is read is its text again (`Line::text` of each line is the text up to `used`), and
//!   `used` ends at a line end, with no line break after it.
//! - Read as it grows, in three reads, the lines are the same as read whole, as is the error.
//! - Reading asks for at most `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use blaze_usercode::trace::{self, BadRecord, Line, Seen};

const TARGET: Target = Target {
    name: "usercode_trace_record",
    check,
    seeds,
    dictionary: &[b"\t", b"\n", b"R\t", b"L\t", b"D\t", b"E\t", b"T\t", b"W\t", b"N\t", b"X\t", b"*", b"clock"],
    max_len: 1 << 16,
    budget: Duration::from_secs(120),
};

/// Whether `line` (without its line break) is a record's line.
fn is_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 2 && b"RLDETWNX".contains(&bytes[0]) && bytes[1] == b'\t' && line[2..].contains('\t')
}

/// What `trace::parse` must say of `text`.
fn model(text: &str) -> Result<(usize, usize), BadRecord> {
    let (mut lines, mut used) = (0, 0);
    for (number, piece) in text.split_inclusive('\n').enumerate() {
        let Some(line) = piece.strip_suffix('\n') else { break };
        if !is_line(line) {
            return Err(BadRecord { line: number + 1, text: line.chars().take(200).collect() });
        }
        lines += 1;
        used += piece.len();
    }
    Ok((lines, used))
}

/// `text` read in reads that end at `ends` in turn, each from where the last one used.
fn grown(text: &str, ends: &[usize]) -> Result<(Vec<Line>, usize), BadRecord> {
    let (mut lines, mut used) = (Vec::new(), 0);
    for &end in ends {
        let (more, more_used) = trace::parse(&text[used..end]).map_err(|bad| BadRecord {
            line: bad.line + text[..used].matches('\n').count(),
            text: bad.text,
        })?;
        lines.extend(more);
        used += more_used;
    }
    Ok((lines, used))
}

/// The last character boundary at or before `at`.
fn boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn check(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let (most, got) = blaze_fuzz::largest_allocation(|| trace::parse(&text));
    assert!(most <= blaze_fuzz::allowed(text.len()), "reading {} bytes asked for {most} at once", text.len());

    let expected = model(&text);
    let said = got.as_ref().map(|(lines, used)| (lines.len(), *used)).map_err(Clone::clone);
    assert_eq!(said, expected, "a record is read exactly when each ended line is a line");

    if let Ok((lines, used)) = &got {
        let again: String = lines.iter().map(Line::text).collect();
        assert_eq!(again, text[..*used], "what is read is its text again");
        assert!(!text[*used..].contains('\n'), "`used` leaves no whole line unread");
    }

    let ends = [boundary(&text, text.len() / 3), boundary(&text, 2 * text.len() / 3), text.len()];
    assert_eq!(grown(&text, &ends), got, "read as it grows, the record is as read whole");
}

/// Records as a sandbox writes them ([`Line::new`] and [`Line::text`]), the last line of some
/// still being written.
fn seeds() -> Vec<Vec<u8>> {
    let lines = [
        Line::new(Seen::Read, "", "/p/foo/build.rs"),
        Line::new(Seen::Looked, "a::b", "/usr/lib/libz.so"),
        Line::new(Seen::Listed, "a::b", "/p/foo/src"),
        Line::new(Seen::Env, "", "CC"),
        Line::new(Seen::Env, "t", trace::ALL_ENV),
        Line::new(Seen::Untraceable, "t", "clock"),
        Line::new(Seen::RefusedWrite, "", "/etc/passwd"),
        Line::new(Seen::RefusedNet, "", "1.1.1.1:443"),
        Line::new(Seen::Read, "", "a path\twith a tab"),
        Line::new(Seen::Read, "a scope\nwith a break", "/x"),
        Line::new(Seen::Read, "ünï", "/p/cødé"),
    ];
    let record: String = lines.iter().map(Line::text).collect();
    let mut torn = record.clone();
    torn.push_str("R\t\t/half");
    let mut bad = record.clone();
    bad.insert_str(0, "Q\tnot\ta letter\n");
    vec![record.into_bytes(), torn.into_bytes(), bad.into_bytes(), b"R\tno second tab\n".to_vec()]
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
