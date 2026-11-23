//! Pure `#![no_std]` sans-IO W3C Server-Sent Events (SSE) streaming parser and formatter.
//!
//! - Incremental byte-chunk streaming via [`SseParser::feed`] with bounded line and event buffers
//!   ([`SseLimits`]).
//! - Handles initial UTF-8 BOM stripping, `\r\n` / `\n` / `\r` line endings, single leading-space
//!   removal after `:`, multi-line `data:` concatenation with `\n`, `: comment` lines, `id:`,
//!   `event:`, and decimal `retry:` fields.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

/// Bounds enforced while parsing untrusted SSE streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SseLimits {
    /// Maximum bytes allowed in a single SSE line (before its line terminator).
    pub most_line_bytes: usize,
    /// Maximum accumulated bytes allowed across fields of a single pending SSE event.
    pub most_event_bytes: usize,
}

impl SseLimits {
    /// Generous default bounds (1 MiB per line, 8 MiB per event).
    pub const USUAL: Self = Self {
        most_line_bytes: 1 << 20,
        most_event_bytes: 8 << 20,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// A single dispatched Server-Sent Event.
pub struct SseEvent<A: Allocator> {
    /// Optional event type from the `event:` field (`None` for the default `"message"` type).
    pub event: Option<String<A>>,
    /// Concatenated `data:` payload lines joined by `\n`.
    pub data: String<A>,
    /// Optional event identifier from an `id:` field present on this event.
    pub id: Option<String<A>>,
    /// Optional reconnection interval in milliseconds from a `retry:` field on this event.
    pub retry_ms: Option<u64>,
}

impl<A: Allocator> fmt::Debug for SseEvent<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseEvent")
            .field("event", &self.event.as_ref().map(String::as_str))
            .field("data", &self.data.as_str())
            .field("id", &self.id.as_ref().map(String::as_str))
            .field("retry_ms", &self.retry_ms)
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<SseEvent<B>> for SseEvent<A> {
    fn eq(&self, other: &SseEvent<B>) -> bool {
        self.event.as_ref().map(String::as_str) == other.event.as_ref().map(String::as_str)
            && self.data.as_str() == other.data.as_str()
            && self.id.as_ref().map(String::as_str) == other.id.as_ref().map(String::as_str)
            && self.retry_ms == other.retry_ms
    }
}

impl<A: Allocator> Eq for SseEvent<A> {}

/// Why parsing an SSE stream failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SseError {
    /// A line exceeded [`SseLimits::most_line_bytes`].
    LineTooLong { limit: usize },
    /// A pending event's accumulated fields exceeded [`SseLimits::most_event_bytes`].
    EventTooLarge { limit: usize },
    /// A line contained invalid UTF-8 bytes.
    InvalidUtf8,
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SseError::LineTooLong { limit } => {
                write!(f, "bstd::sse: line length exceeds limit of {limit} bytes")
            }
            SseError::EventTooLarge { limit } => {
                write!(f, "bstd::sse: event payload exceeds limit of {limit} bytes")
            }
            SseError::InvalidUtf8 => f.write_str("bstd::sse: invalid UTF-8 in stream"),
            SseError::Alloc(err) => write!(f, "bstd::sse: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for SseError {}

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Incremental sans-IO W3C Server-Sent Events parser.
pub struct SseParser<A: Allocator + Copy> {
    limits: SseLimits,
    alloc: A,
    bom_buf: [u8; 3],
    bom_len: usize,
    bom_done: bool,
    saw_cr: bool,
    line_buf: Vec<u8, A>,
    event_buf: Option<String<A>>,
    data_buf: String<A>,
    has_data: bool,
    event_id: Option<String<A>>,
    last_event_id: Option<String<A>>,
    event_retry_ms: Option<u64>,
    last_retry_ms: Option<u64>,
}

impl<A: Allocator + Copy> SseParser<A> {
    /// Create a new streaming SSE parser using `limits` and `alloc`.
    #[must_use]
    pub fn new_in(limits: SseLimits, alloc: A) -> Self {
        Self {
            limits,
            alloc,
            bom_buf: [0; 3],
            bom_len: 0,
            bom_done: false,
            saw_cr: false,
            line_buf: Vec::new_in(alloc),
            event_buf: None,
            data_buf: String::new_in(alloc),
            has_data: false,
            event_id: None,
            last_event_id: None,
            event_retry_ms: None,
            last_retry_ms: None,
        }
    }

    /// Most recent `id:` value observed across the stream, per the W3C EventSource algorithm.
    #[must_use]
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_ref().map(String::as_str)
    }

    /// Most recent `retry:` interval in milliseconds observed across the stream.
    #[must_use]
    pub fn last_retry_ms(&self) -> Option<u64> {
        self.last_retry_ms
    }

    /// Feed an arbitrary chunk of bytes into the parser, appending any completed events to `out`.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        out: &mut Vec<SseEvent<A>, A>,
    ) -> Result<(), SseError> {
        self.feed_planted(bytes, out, false)
    }

    fn feed_planted(
        &mut self,
        bytes: &[u8],
        out: &mut Vec<SseEvent<A>, A>,
        plant_skip_space_strip: bool,
    ) -> Result<(), SseError> {
        for &b in bytes {
            if !self.bom_done {
                if UTF8_BOM.get(self.bom_len) == Some(&b) {
                    if let Some(slot) = self.bom_buf.get_mut(self.bom_len) {
                        *slot = b;
                    }
                    self.bom_len += 1;
                    if self.bom_len == UTF8_BOM.len() {
                        self.bom_done = true;
                        self.bom_len = 0;
                    }
                    continue;
                }
                self.bom_done = true;
                let replay_len = self.bom_len;
                self.bom_len = 0;
                let replay = self.bom_buf;
                if let Some(prefix) = replay.get(..replay_len) {
                    for &prev in prefix {
                        self.push_byte(prev, out, plant_skip_space_strip)?;
                    }
                }
            }
            self.push_byte(b, out, plant_skip_space_strip)?;
        }
        Ok(())
    }

    fn push_byte(
        &mut self,
        b: u8,
        out: &mut Vec<SseEvent<A>, A>,
        plant_skip_space_strip: bool,
    ) -> Result<(), SseError> {
        if self.saw_cr {
            self.saw_cr = false;
            if b == b'\n' {
                return Ok(());
            }
        }
        if b == b'\r' {
            self.saw_cr = true;
            return self.process_line(out, plant_skip_space_strip);
        }
        if b == b'\n' {
            return self.process_line(out, plant_skip_space_strip);
        }
        if self.line_buf.len() >= self.limits.most_line_bytes {
            return Err(SseError::LineTooLong { limit: self.limits.most_line_bytes });
        }
        self.line_buf.try_push(b).map_err(|r| SseError::Alloc(r.why))?;
        Ok(())
    }

    fn process_line(
        &mut self,
        out: &mut Vec<SseEvent<A>, A>,
        plant_skip_space_strip: bool,
    ) -> Result<(), SseError> {
        if self.line_buf.is_empty() {
            return self.dispatch_event(out);
        }

        let result = self.interpret_line(plant_skip_space_strip);
        self.line_buf.clear();
        result
    }

    fn interpret_line(&mut self, plant_skip_space_strip: bool) -> Result<(), SseError> {
        let line = core::str::from_utf8(self.line_buf.as_slice())
            .map_err(|_| SseError::InvalidUtf8)?;

        if line.starts_with(':') {
            return Ok(());
        }

        let (field, raw_value) = match line.find(':') {
            Some(colon) => {
                let name = line.get(..colon).unwrap_or("");
                let rest = line.get(colon + 1..).unwrap_or("");
                let val = if !plant_skip_space_strip {
                    rest.strip_prefix(' ').unwrap_or(rest)
                } else {
                    rest
                };
                (name, val)
            }
            None => (line, ""),
        };

        match field {
            "event" => {
                let current_other = self.data_buf.len()
                    + self.event_id.as_ref().map_or(0, String::len);
                if current_other.saturating_add(raw_value.len()) > self.limits.most_event_bytes {
                    return Err(SseError::EventTooLarge { limit: self.limits.most_event_bytes });
                }
                if raw_value.is_empty() {
                    self.event_buf = None;
                } else {
                    let s = String::try_from_str_in(raw_value, self.alloc)
                        .map_err(SseError::Alloc)?;
                    self.event_buf = Some(s);
                }
            }
            "data" => {
                let sep_len = if self.has_data { 1usize } else { 0usize };
                let next_total = self.event_bytes()
                    .saturating_add(sep_len)
                    .saturating_add(raw_value.len());
                if next_total > self.limits.most_event_bytes {
                    return Err(SseError::EventTooLarge { limit: self.limits.most_event_bytes });
                }
                if self.has_data {
                    self.data_buf.try_push('\n').map_err(SseError::Alloc)?;
                }
                self.data_buf.try_push_str(raw_value).map_err(SseError::Alloc)?;
                self.has_data = true;
            }
            "id" => {
                if !raw_value.as_bytes().contains(&0) {
                    let current_other = self.data_buf.len()
                        + self.event_buf.as_ref().map_or(0, String::len);
                    if current_other.saturating_add(raw_value.len()) > self.limits.most_event_bytes {
                        return Err(SseError::EventTooLarge { limit: self.limits.most_event_bytes });
                    }
                    let id_for_event = String::try_from_str_in(raw_value, self.alloc)
                        .map_err(SseError::Alloc)?;
                    let id_for_stream = String::try_from_str_in(raw_value, self.alloc)
                        .map_err(SseError::Alloc)?;
                    self.event_id = Some(id_for_event);
                    self.last_event_id = Some(id_for_stream);
                }
            }
            "retry" => {
                if !raw_value.is_empty() && raw_value.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(ms) = raw_value.parse::<u64>() {
                        self.event_retry_ms = Some(ms);
                        self.last_retry_ms = Some(ms);
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn event_bytes(&self) -> usize {
        self.data_buf.len()
            + self.event_buf.as_ref().map_or(0, String::len)
            + self.event_id.as_ref().map_or(0, String::len)
    }

    fn dispatch_event(&mut self, out: &mut Vec<SseEvent<A>, A>) -> Result<(), SseError> {
        let event = self.event_buf.take();
        let id = self.event_id.take();
        let retry_ms = self.event_retry_ms.take();
        if !self.has_data {
            self.data_buf.clear();
            return Ok(());
        }
        self.has_data = false;
        let data = core::mem::replace(&mut self.data_buf, String::new_in(self.alloc));
        out.try_push(SseEvent { event, data, id, retry_ms })
            .map_err(|r| SseError::Alloc(r.why))?;
        Ok(())
    }
}

/// Format a single Server-Sent Event into `out` (ending with a blank line `\n`).
pub fn format_event_into<W: fmt::Write>(
    event: Option<&str>,
    data: &str,
    id: Option<&str>,
    retry_ms: Option<u64>,
    out: &mut W,
) -> fmt::Result {
    if let Some(ev) = event {
        out.write_str("event: ")?;
        out.write_str(ev)?;
        out.write_char('\n')?;
    }
    if let Some(id_str) = id {
        out.write_str("id: ")?;
        out.write_str(id_str)?;
        out.write_char('\n')?;
    }
    if let Some(ms) = retry_ms {
        write!(out, "retry: {ms}\n")?;
    }
    for line in data.split('\n') {
        out.write_str("data: ")?;
        out.write_str(line)?;
        out.write_char('\n')?;
    }
    out.write_char('\n')
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::{SseError, SseEvent, SseLimits, SseParser, format_event_into};

    #[test]
    fn llm_style_streaming_chunks_and_bom() {
        let h = heap();
        let alloc = h.alloc();
        let raw = concat!(
            "\u{FEFF}",
            ": heartbeat comment\r\n",
            "event: message_start\r\n",
            "id: evt_01\r\n",
            "retry: 3000\r\n",
            "data: {\"type\":\"message_start\"}\r\n",
            "\r\n",
            "event: content_block_delta\n",
            "data: line 1\n",
            "data:  leading space kept\n",
            "\n",
            "data: [DONE]\r\r"
        );
        let bytes = raw.as_bytes();
        // Feed 1 byte at a time to stress BOM, CRLF, LF, and CR boundaries.
        let mut parser = SseParser::new_in(SseLimits::SHIPPED, alloc);
        let mut events = OwnVec::new_in(alloc);
        for chunk in bytes.chunks(1) {
            parser.feed(chunk, &mut events).expect("feed byte");
        }

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_ref().map(OwnString::as_str), Some("message_start"));
        assert_eq!(events[0].id.as_ref().map(OwnString::as_str), Some("evt_01"));
        assert_eq!(events[0].retry_ms, Some(3000));
        assert_eq!(events[0].data.as_str(), "{\"type\":\"message_start\"}");

        assert_eq!(events[1].event.as_ref().map(OwnString::as_str), Some("content_block_delta"));
        assert_eq!(events[1].id, None);
        assert_eq!(events[1].data.as_str(), "line 1\n leading space kept");

        assert_eq!(events[2].event, None);
        assert_eq!(events[2].data.as_str(), "[DONE]");
        assert_eq!(parser.last_event_id(), Some("evt_01"));
        assert_eq!(parser.last_retry_ms(), Some(3000));
    }

    #[test]
    fn sse_limits_and_edge_cases() {
        let h = heap();
        let alloc = h.alloc();
        let mut parser = SseParser::new_in(
            SseLimits { most_line_bytes: 12, most_event_bytes: 64 },
            alloc,
        );
        let mut out = OwnVec::new_in(alloc);
        let err = parser.feed(b"data: 1234567890\n\n", &mut out);
        assert_eq!(err, Err(SseError::LineTooLong { limit: 12 }));

        // Empty data line dispatches empty string; comment-only block dispatches nothing.
        let mut p2 = SseParser::new_in(SseLimits::SHIPPED, alloc);
        p2.feed(b": only comment\n\ndata:\n\n", &mut out).expect("feed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.as_str(), "");
    }

    fn draw_events<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        alloc: A,
    ) -> OwnVec<SseEvent<A>, A> {
        const EVENTS: &[&str] = &["token", "delta", "done", "status"];
        const IDS: &[&str] = &["1", "evt-42", "abc_9", ""];
        const PIECES: &[&str] = &["", "hello", " world", "a:b", "  spaced", "\n", "café", "{}"];
        let count = tape.int_in(1usize..=6, "events");
        let mut list = OwnVec::new_in(alloc);
        for _ in 0..count {
            let event = if tape.coin("has_event") {
                let idx = tape.index(EVENTS.len(), "ev");
                Some(OwnString::from_str_in(EVENTS[idx], alloc))
            } else {
                None
            };
            let id = if tape.coin("has_id") {
                let idx = tape.index(IDS.len(), "id");
                Some(OwnString::from_str_in(IDS[idx], alloc))
            } else {
                None
            };
            let retry_ms = if tape.coin("has_retry") {
                Some(tape.int_in(0u64..=60_000, "retry"))
            } else {
                None
            };
            let parts = tape.int_in(0usize..=4, "parts");
            let mut data = OwnString::new_in(alloc);
            for _ in 0..parts {
                let idx = tape.index(PIECES.len(), "piece");
                data.push_str(PIECES[idx]);
            }
            list.push(SseEvent { event, data, id, retry_ms });
        }
        list
    }

    fn check_sse_round_trip<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        events: &[SseEvent<A>],
        plant_skip_space_strip: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut wire = OwnString::new_in(alloc);
        if tape.coin("with_bom") {
            wire.push('\u{FEFF}');
        }
        for ev in events {
            format_event_into(
                ev.event.as_ref().map(OwnString::as_str),
                ev.data.as_str(),
                ev.id.as_ref().map(OwnString::as_str),
                ev.retry_ms,
                &mut wire,
            )
            .expect("format");
        }

        let mut parser = SseParser::new_in(SseLimits::SHIPPED, alloc);
        let mut decoded = OwnVec::new_in(alloc);
        let raw = wire.as_bytes();
        let mut pos = 0usize;
        while pos < raw.len() {
            let max_step = (raw.len() - pos).min(17);
            let step = tape.int_in(1usize..=max_step, "chunk");
            let end = (pos + step).min(raw.len());
            parser
                .feed_planted(&raw[pos..end], &mut decoded, plant_skip_space_strip)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "feed failed: {e}").as_str().into()))?;
            pos = end;
        }

        if decoded.len() != events.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "event count mismatch: expected {}, got {}", events.len(), decoded.len())
                    .as_str()
                    .into(),
            ));
        }
        for (i, (got, exp)) in decoded.iter().zip(events.iter()).enumerate() {
            if got != exp {
                return Err(Broke::Oracle(
                    format_in!(alloc, "event {i} mismatch: expected {exp:?}, got {got:?}")
                        .as_str()
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// The invariant: chunked SSE parsing matches whole-buffer parsing and strips leading event field spaces.
    #[test]
    fn property_sse_chunked_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let events = draw_events(tape, h.alloc());
            check_sse_round_trip(tape, events.as_slice(), false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_missing_space_strip() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let events = draw_events(tape, h.alloc());
            check_sse_round_trip(tape, events.as_slice(), plant, h.alloc())
        });
    }
}
