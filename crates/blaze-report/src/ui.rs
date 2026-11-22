//! Blaze's clean terminal display & layout rendering library (`blaze-report::ui`).
//!
//! Replaces raw unwrapped CLI stderr walls with:
//! - **Human-readable units**: `fmt_bytes` (`705.8 MiB`, `2.97 GiB`), `fmt_count` (`1,234`),
//!   `fmt_duration_ns` (`452 µs`, `4.02 s`), `fmt_rate_per_core` (`143.1 MB/s/core`), and
//!   `fmt_delta_pct` (`-94.2%`).
//! - **ANSI-aware word wrapping** (`wrap_styled`): wraps cleanly on word boundaries to terminal
//!   width with hanging indents, never breaking mid-word.
//! - **Collapsible `NoticeGroup`**: turns N repetitive notices (such as multiple stale cache
//!   stores) into one crisp summary block with a single action hint.
//! - **Aligned `Table`** & **styled `RunFooter`** using [`crate::palette`]'s WCAG-AA semantic tokens.
//! - **Previous-run ledger (`RunRecord` / `RunComparison`)**: deterministic binary persistence
//!   and run-over-run trend tables (`duration`, `cache hit %`, `memory`, `throughput/core`).

use core::fmt::{self, Write as _};

use crate::palette::{Depth, Ground, Token, sgr};
use crate::render::Caps;

/// Formats a byte count cleanly with binary IEC units (`B`, `KiB`, `MiB`, `GiB`, `TiB`).
pub fn fmt_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes < TIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    }
}

/// Formats an integer with ASCII thousands separators (`1,234,567`).
pub fn fmt_count(n: u64) -> String {
    let raw = n.to_string();
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Formats a nanosecond duration cleanly (`ns`, `µs`, `ms`, `s`, `m s`).
pub fn fmt_duration_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.1} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1} ms", ns as f64 / 1_000_000.0)
    } else if ns < 60_000_000_000 {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    } else {
        let secs = ns / 1_000_000_000;
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem:02}s")
    }
}

/// Formats per-core throughput (`MB/s/core` or `GB/s/core`) explicitly naming core count (Rule 84).
pub fn fmt_rate_per_core(bytes: u64, ns: u64, cores: u32) -> String {
    let c = cores.max(1) as f64;
    let secs = (ns.max(1) as f64) / 1_000_000_000.0;
    let mb_per_s_core = (bytes as f64 / 1_000_000.0) / secs / c;
    if mb_per_s_core >= 1000.0 {
        format!("{:.2} GB/s/core ({} cores)", mb_per_s_core / 1000.0, cores.max(1))
    } else {
        format!("{:.1} MB/s/core ({} cores)", mb_per_s_core, cores.max(1))
    }
}

/// Formats a percentage change from `prev` to `cur` (e.g. `"-42.5%"` or `"+12.0%"`).
pub fn fmt_delta_pct(prev: u64, cur: u64) -> String {
    if prev == 0 {
        return String::from("n/a");
    }
    let delta = ((cur as f64 - prev as f64) / prev as f64) * 100.0;
    if delta >= 0.0 {
        format!("+{delta:.1}%")
    } else {
        format!("{delta:.1}%")
    }
}

/// Computes the visible column width of `s`, ignoring ANSI CSI (`\x1b[...m`) escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for esc in chars.by_ref() {
                if esc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

/// Wraps `text` cleanly on word boundaries to `max_cols`, prefixing the first line with
/// `first_prefix` and continuation lines with `hang_prefix`. Never splits mid-word unless a
/// single token exceeds the available line width.
pub fn wrap_words(text: &str, max_cols: usize, first_prefix: &str, hang_prefix: &str) -> String {
    let limit = max_cols.max(24);
    let first_w = visible_width(first_prefix);
    let hang_w = visible_width(hang_prefix);
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str(first_prefix);
    let mut col = first_w;
    let mut first_word_on_line = true;

    for word in text.split_whitespace() {
        let ww = visible_width(word);
        let need = if first_word_on_line { ww } else { 1 + ww };
        if !first_word_on_line && col + need > limit {
            out.push('\n');
            out.push_str(hang_prefix);
            col = hang_w;
            first_word_on_line = true;
        }
        if !first_word_on_line {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += ww;
        first_word_on_line = false;
    }
    out
}

/// Lightweight styled writer over `blaze-report`'s WCAG-AA palette (`Depth` + `Ground`).
pub struct Theme {
    pub depth: Depth,
    pub ground: Ground,
    pub width: usize,
}

impl Theme {
    pub fn from_caps(caps: &Caps) -> Theme {
        Theme {
            depth: if caps.color { Depth::True } else { Depth::None },
            ground: Ground::Dark,
            width: caps.width.map_or(88, usize::from).clamp(40, 120),
        }
    }

    pub const PLAIN: Theme = Theme { depth: Depth::None, ground: Ground::Dark, width: 88 };

    /// Wraps `text` in the ANSI SGR sequence for `token`, or returns plain `text` when `Depth::None`.
    pub fn paint(&self, token: Token, text: &str) -> String {
        let code = sgr(token, self.depth, self.ground);
        if code.is_empty() {
            String::from(text)
        } else {
            format!("\x1b[{code}m{text}\x1b[0m")
        }
    }

    /// Renders a clean compact notice line with semantic badge + word-wrapped body.
    pub fn notice(&self, badge: &str, token: Token, body: &str) -> String {
        let prefix = format!("  {}  ", self.paint(token, badge));
        let hang = " ".repeat(2 + visible_width(badge) + 2);
        wrap_words(body, self.width, &prefix, &hang)
    }

    /// Renders a collapsible group of related items (e.g., stale cache stores) as one scannable
    /// block instead of N repetitive paragraphs.
    pub fn notice_group(
        &self,
        badge: &str,
        token: Token,
        headline: &str,
        items: &[(&str, &str)],
        action: Option<&str>,
    ) -> String {
        let mut out = String::new();
        let header = format!("  {}  {}", self.paint(token, badge), self.paint(Token::Emphasis, headline));
        out.push_str(&header);
        out.push('\n');

        let max_left = items.iter().map(|(l, _)| visible_width(l)).max().unwrap_or(0);
        let gutter = self.paint(Token::Dim, "│");
        for (idx, (left, right)) in items.iter().enumerate() {
            let branch = if idx + 1 == items.len() && action.is_none() {
                self.paint(Token::Dim, "└")
            } else {
                gutter.clone()
            };
            let pad = max_left.saturating_sub(visible_width(left));
            let _ = write!(
                out,
                "  {branch} {}{}  {}\n",
                self.paint(Token::Path, left),
                " ".repeat(pad),
                self.paint(Token::Dim, right)
            );
        }
        if let Some(hint) = action {
            let corner = self.paint(Token::Dim, "└");
            let first = format!("  {corner} ");
            let hang = "    ";
            out.push_str(&wrap_words(hint, self.width, &first, hang));
            out.push('\n');
        }
        out
    }

    /// Renders a crisp, low-noise run summary footer (`blaze run · 106 compiled · 0 cached · 4.02 s`).
    pub fn run_footer(
        &self,
        command: &str,
        compiled: usize,
        cached: usize,
        unkeyed: usize,
        duration_ns: Option<u64>,
    ) -> String {
        let dot = self.paint(Token::Dim, "·");
        let mut parts = Vec::new();
        parts.push(self.paint(Token::Success, command));
        if compiled > 0 {
            parts.push(format!(
                "{} {}",
                self.paint(Token::Number, &fmt_count(compiled as u64)),
                self.paint(Token::Dim, "compiled")
            ));
        }
        if cached > 0 || compiled == 0 {
            parts.push(format!(
                "{} {}",
                self.paint(Token::Cached, &fmt_count(cached as u64)),
                self.paint(Token::Dim, "cached")
            ));
        }
        if unkeyed > 0 {
            parts.push(format!(
                "{} {}",
                self.paint(Token::Warning, &fmt_count(unkeyed as u64)),
                self.paint(Token::Dim, "unkeyed")
            ));
        }
        if let Some(ns) = duration_ns {
            parts.push(self.paint(Token::Dim, &fmt_duration_ns(ns)));
        }
        let sep = format!(" {dot} ");
        format!("  {}", parts.join(&sep))
    }

    /// Renders an auto-aligned multi-column table (headers + rows), respecting ANSI widths.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>], right_align: &[bool]) -> String {
        let cols = headers.len();
        if cols == 0 {
            return String::new();
        }
        let mut widths = vec![0usize; cols];
        for (c, h) in headers.iter().enumerate() {
            widths[c] = visible_width(h);
        }
        for row in rows {
            for (c, cell) in row.iter().take(cols).enumerate() {
                widths[c] = widths[c].max(visible_width(cell));
            }
        }
        let mut out = String::new();
        out.push_str("  ");
        for (c, h) in headers.iter().enumerate() {
            if c > 0 {
                out.push_str("  ");
            }
            let pad = widths[c].saturating_sub(visible_width(h));
            if right_align.get(c).copied().unwrap_or(false) {
                out.push_str(&" ".repeat(pad));
                out.push_str(&self.paint(Token::Dim, h));
            } else {
                out.push_str(&self.paint(Token::Dim, h));
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push('\n');
        let rule_len: usize = widths.iter().sum::<usize>() + (cols.saturating_sub(1)) * 2;
        let _ = writeln!(out, "  {}", self.paint(Token::Dim, &"─".repeat(rule_len)));
        for row in rows {
            out.push_str("  ");
            for c in 0..cols {
                if c > 0 {
                    out.push_str("  ");
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let pad = widths[c].saturating_sub(visible_width(cell));
                if right_align.get(c).copied().unwrap_or(false) {
                    out.push_str(&" ".repeat(pad));
                    out.push_str(cell);
                } else {
                    out.push_str(cell);
                    out.push_str(&" ".repeat(pad));
                }
            }
            out.push('\n');
        }
        out
    }
}

// ---- Previous-Runs Telemetry Ledger (`RunRecord` / `RunComparison`) -------------------------

/// Compact summary of one historical Blaze run, stored for run-over-run trend inspection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunRecord {
    pub run_id: u64,
    pub timestamp_unix_s: u64,
    pub duration_ns: u64,
    pub compiled: u32,
    pub cached: u32,
    pub peak_rss_bytes: u64,
    pub bytes_processed: u64,
    pub cores: u32,
}

impl RunRecord {
    pub const WIRE_LEN: usize = 56;

    /// Encodes this record as 56 little-endian bytes (fixed-width for O(1) slice indexing).
    pub fn to_bytes(self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0..8].copy_from_slice(&self.run_id.to_le_bytes());
        b[8..16].copy_from_slice(&self.timestamp_unix_s.to_le_bytes());
        b[16..24].copy_from_slice(&self.duration_ns.to_le_bytes());
        b[24..28].copy_from_slice(&self.compiled.to_le_bytes());
        b[28..32].copy_from_slice(&self.cached.to_le_bytes());
        b[32..40].copy_from_slice(&self.peak_rss_bytes.to_le_bytes());
        b[40..48].copy_from_slice(&self.bytes_processed.to_le_bytes());
        b[48..52].copy_from_slice(&self.cores.to_le_bytes());
        // 4 bytes reserved (0)
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<RunRecord> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        Some(RunRecord {
            run_id: u64::from_le_bytes(b[0..8].try_into().ok()?),
            timestamp_unix_s: u64::from_le_bytes(b[8..16].try_into().ok()?),
            duration_ns: u64::from_le_bytes(b[16..24].try_into().ok()?),
            compiled: u32::from_le_bytes(b[24..28].try_into().ok()?),
            cached: u32::from_le_bytes(b[28..32].try_into().ok()?),
            peak_rss_bytes: u64::from_le_bytes(b[32..40].try_into().ok()?),
            bytes_processed: u64::from_le_bytes(b[40..48].try_into().ok()?),
            cores: u32::from_le_bytes(b[48..52].try_into().ok()?),
        })
    }

    /// Cache hit percentage (`0..=100`).
    pub fn cache_hit_pct(&self) -> u32 {
        let total = u64::from(self.compiled) + u64::from(self.cached);
        if total == 0 {
            100
        } else {
            ((u64::from(self.cached) * 100) / total) as u32
        }
    }
}

/// Renders a clean comparison table of recent runs, highlighting improvements and regressions.
pub fn render_run_history(theme: &Theme, runs: &[RunRecord]) -> String {
    let headers = ["run", "time", "delta", "cache", "rss", "rate/core"];
    let right_align = [true, true, true, true, true, true];
    let mut rows = Vec::with_capacity(runs.len());
    for (idx, r) in runs.iter().enumerate() {
        let delta = if idx == 0 {
            theme.paint(Token::Dim, "base")
        } else {
            let prev = runs[idx - 1].duration_ns;
            let text = fmt_delta_pct(prev, r.duration_ns);
            let tok = if r.duration_ns <= prev { Token::Success } else { Token::Failure };
            theme.paint(tok, &text)
        };
        let cache = format!("{}% ({}/{})", r.cache_hit_pct(), r.cached, r.compiled + r.cached);
        let rate = if r.bytes_processed > 0 {
            fmt_rate_per_core(r.bytes_processed, r.duration_ns, r.cores)
        } else {
            String::from("—")
        };
        rows.push(vec![
            format!("#{}", r.run_id),
            theme.paint(Token::Number, &fmt_duration_ns(r.duration_ns)),
            delta,
            theme.paint(Token::Cached, &cache),
            fmt_bytes(r.peak_rss_bytes),
            rate,
        ]);
    }
    theme.table(&headers, &rows, &right_align)
}

impl fmt::Display for RunRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run #{} · {} · {} compiled / {} cached · rss {}",
            self.run_id,
            fmt_duration_ns(self.duration_ns),
            self.compiled,
            self.cached,
            fmt_bytes(self.peak_rss_bytes)
        )
    }
}
