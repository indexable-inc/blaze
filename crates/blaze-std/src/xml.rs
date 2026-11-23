//! Pure `#![no_std]`, bounded, 100% iterative, XXE-proof XML 1.0 pull parser and canonical writer.
//!
//! - **XXE & Billion-Laughs defense**: explicitly refuses every `<!ENTITY` declaration (both at
//!   top level and inside `<!DOCTYPE ... [ ... ]>` internal subsets) with
//!   [`XmlError::EntityDeclForbidden`], and rejects all unknown named entities. Only the 5 XML 1.0
//!   predefined entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`) and numeric character
//!   references (`&#dec;`, `&#xhex;`) validated as XML 1.0 scalar values are accepted.
//! - **100% iterative**: uses explicit stacks in the caller's allocator (`A: Allocator + Copy`)
//!   with zero recursion, bounded by [`XmlLimits`].
//! - **Canonical writer**: [`XmlWriter`] tracks open elements on an explicit stack, escapes text
//!   and attribute values with XML 1.0 entities, rejects invalid names, duplicate attributes,
//!   `]]>` in CDATA, `--` in comments, and unclosed elements on [`XmlWriter::finish`].

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

/// Bounds enforced by [`XmlReader`] while parsing untrusted XML input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct XmlLimits {
    /// Maximum input length in bytes.
    pub most_bytes: usize,
    /// Maximum element nesting depth.
    pub most_depth: usize,
    /// Maximum number of attributes on a single element.
    pub most_attrs: usize,
    /// Maximum number of events yielded from a single document.
    pub most_events: usize,
}

impl XmlLimits {
    /// Default bounds: 8 MiB input, 64 nesting depth, 128 attrs per element, 250,000 events.
    pub const USUAL: Self = Self {
        most_bytes: 8 << 20,
        most_depth: 64,
        most_attrs: 128,
        most_events: 250_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed failure returned by [`XmlReader`] and [`XmlWriter`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmlError {
    /// Input byte length exceeded [`XmlLimits::most_bytes`].
    InputTooLarge { bytes: usize, limit: usize },
    /// Element nesting depth exceeded [`XmlLimits::most_depth`].
    DepthLimitExceeded { limit: usize, offset: usize },
    /// Attribute count on an element exceeded [`XmlLimits::most_attrs`].
    TooManyAttrs { limit: usize, offset: usize },
    /// Emitted event count exceeded [`XmlLimits::most_events`].
    TooManyEvents { limit: usize, offset: usize },
    /// A `<!ENTITY` declaration was encountered; entity declarations are forbidden (XXE defense).
    EntityDeclForbidden,
    /// An unknown named entity reference (outside `amp`, `lt`, `gt`, `quot`, `apos`) was found.
    UnknownEntity { offset: usize },
    /// A malformed or out-of-range entity or numeric character reference was found.
    InvalidEntity { offset: usize },
    /// Input ended unexpectedly inside markup or a literal.
    UnexpectedEof { offset: usize },
    /// An element, attribute, or processing-instruction name violated XML 1.0 `Name` syntax.
    InvalidName { offset: usize },
    /// Malformed attribute syntax or disallowed character in an attribute value.
    InvalidAttr { offset: usize },
    /// Duplicate attribute name on the same element.
    DuplicateAttr { offset: usize },
    /// A closing tag did not match the most recently opened element.
    MismatchedEndTag { offset: usize },
    /// A closing tag appeared with no matching open element on the stack.
    UnexpectedEndTag { offset: usize },
    /// One or more opened elements were never closed before EOF / [`XmlWriter::finish`].
    UnclosedElement { depth: usize, offset: usize },
    /// Comment body contained `--`, ended with `-`, or contained an invalid XML 1.0 character.
    InvalidComment { offset: usize },
    /// CDATA section contained `]]>` or an invalid XML 1.0 character.
    InvalidCData { offset: usize },
    /// Malformed processing instruction (`<?target ...?>`).
    InvalidPi { offset: usize },
    /// Malformed `<!DOCTYPE ...>` declaration.
    InvalidDoctype { offset: usize },
    /// Unexpected markup syntax or invalid XML 1.0 character.
    UnexpectedSyntax { offset: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            XmlError::InputTooLarge { bytes, limit } => {
                write!(f, "bstd::xml: input size {bytes} bytes exceeds limit {limit}")
            }
            XmlError::DepthLimitExceeded { limit, offset } => {
                write!(f, "bstd::xml: byte {offset}: nesting depth exceeds limit {limit}")
            }
            XmlError::TooManyAttrs { limit, offset } => {
                write!(f, "bstd::xml: byte {offset}: attribute count exceeds limit {limit}")
            }
            XmlError::TooManyEvents { limit, offset } => {
                write!(f, "bstd::xml: byte {offset}: event count exceeds limit {limit}")
            }
            XmlError::EntityDeclForbidden => {
                f.write_str("bstd::xml: <!ENTITY declarations are forbidden")
            }
            XmlError::UnknownEntity { offset } => {
                write!(f, "bstd::xml: byte {offset}: unknown named XML entity")
            }
            XmlError::InvalidEntity { offset } => {
                write!(f, "bstd::xml: byte {offset}: malformed or invalid XML entity reference")
            }
            XmlError::UnexpectedEof { offset } => {
                write!(f, "bstd::xml: byte {offset}: unexpected end of input")
            }
            XmlError::InvalidName { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML name")
            }
            XmlError::InvalidAttr { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML attribute")
            }
            XmlError::DuplicateAttr { offset } => {
                write!(f, "bstd::xml: byte {offset}: duplicate attribute name")
            }
            XmlError::MismatchedEndTag { offset } => {
                write!(f, "bstd::xml: byte {offset}: closing tag does not match open element")
            }
            XmlError::UnexpectedEndTag { offset } => {
                write!(f, "bstd::xml: byte {offset}: unexpected closing tag with no open element")
            }
            XmlError::UnclosedElement { depth, offset } => {
                write!(f, "bstd::xml: byte {offset}: {depth} unclosed element(s) remaining")
            }
            XmlError::InvalidComment { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML comment")
            }
            XmlError::InvalidCData { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML CDATA section")
            }
            XmlError::InvalidPi { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML processing instruction")
            }
            XmlError::InvalidDoctype { offset } => {
                write!(f, "bstd::xml: byte {offset}: invalid XML DOCTYPE declaration")
            }
            XmlError::UnexpectedSyntax { offset } => {
                write!(f, "bstd::xml: byte {offset}: unexpected XML syntax")
            }
            XmlError::Alloc(err) => write!(f, "bstd::xml: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for XmlError {}

/// A single decoded XML attribute (`name="value"` or `name='value'`).
pub struct XmlAttr<'a, A: Allocator> {
    /// Borrowed attribute name from the source document.
    pub name: &'a str,
    /// Unescaped attribute value allocated in `A`.
    pub value: String<A>,
}

impl<A: Allocator> PartialEq for XmlAttr<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.value.as_str() == other.value.as_str()
    }
}

impl<A: Allocator> Eq for XmlAttr<'_, A> {}

impl<A: Allocator> fmt::Debug for XmlAttr<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XmlAttr")
            .field("name", &self.name)
            .field("value", &self.value.as_str())
            .finish()
    }
}

/// One event produced by [`XmlReader::next_event`].
pub enum XmlEvent<'a, A: Allocator> {
    /// Opening element `<name attrs>` (`empty: false`) or empty element `<name attrs/>`
    /// (`empty: true`).
    StartElement { name: &'a str, attrs: Vec<XmlAttr<'a, A>, A>, empty: bool },
    /// Closing element `</name>`, verified against the reader's explicit open-element stack.
    EndElement { name: &'a str },
    /// Unescaped XML character data outside CDATA sections.
    Text(String<A>),
    /// Verbatim `<![CDATA[...]]>` payload slice borrowed from the source document.
    CData(&'a str),
    /// Verbatim `<!--...-->` comment payload slice borrowed from the source document.
    Comment(&'a str),
    /// Processing instruction or XML declaration (`<?target data?>`).
    ProcessingInstruction { target: &'a str, data: &'a str },
}

impl<A: Allocator> PartialEq for XmlEvent<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        match self {
            XmlEvent::StartElement { name, attrs, empty } => match other {
                XmlEvent::StartElement { name: o_name, attrs: o_attrs, empty: o_empty } => {
                    name == o_name && attrs.as_slice() == o_attrs.as_slice() && empty == o_empty
                }
                XmlEvent::EndElement { .. }
                | XmlEvent::Text(_)
                | XmlEvent::CData(_)
                | XmlEvent::Comment(_)
                | XmlEvent::ProcessingInstruction { .. } => false,
            },
            XmlEvent::EndElement { name } => match other {
                XmlEvent::EndElement { name: o_name } => name == o_name,
                XmlEvent::StartElement { .. }
                | XmlEvent::Text(_)
                | XmlEvent::CData(_)
                | XmlEvent::Comment(_)
                | XmlEvent::ProcessingInstruction { .. } => false,
            },
            XmlEvent::Text(text) => match other {
                XmlEvent::Text(o_text) => text.as_str() == o_text.as_str(),
                XmlEvent::StartElement { .. }
                | XmlEvent::EndElement { .. }
                | XmlEvent::CData(_)
                | XmlEvent::Comment(_)
                | XmlEvent::ProcessingInstruction { .. } => false,
            },
            XmlEvent::CData(data) => match other {
                XmlEvent::CData(o_data) => data == o_data,
                XmlEvent::StartElement { .. }
                | XmlEvent::EndElement { .. }
                | XmlEvent::Text(_)
                | XmlEvent::Comment(_)
                | XmlEvent::ProcessingInstruction { .. } => false,
            },
            XmlEvent::Comment(body) => match other {
                XmlEvent::Comment(o_body) => body == o_body,
                XmlEvent::StartElement { .. }
                | XmlEvent::EndElement { .. }
                | XmlEvent::Text(_)
                | XmlEvent::CData(_)
                | XmlEvent::ProcessingInstruction { .. } => false,
            },
            XmlEvent::ProcessingInstruction { target, data } => match other {
                XmlEvent::ProcessingInstruction { target: o_target, data: o_data } => {
                    target == o_target && data == o_data
                }
                XmlEvent::StartElement { .. }
                | XmlEvent::EndElement { .. }
                | XmlEvent::Text(_)
                | XmlEvent::CData(_)
                | XmlEvent::Comment(_) => false,
            },
        }
    }
}

impl<A: Allocator> Eq for XmlEvent<'_, A> {}

impl<A: Allocator> fmt::Debug for XmlEvent<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XmlEvent::StartElement { name, attrs, empty } => f
                .debug_struct("StartElement")
                .field("name", name)
                .field("attrs", &attrs.as_slice())
                .field("empty", empty)
                .finish(),
            XmlEvent::EndElement { name } => {
                f.debug_struct("EndElement").field("name", name).finish()
            }
            XmlEvent::Text(s) => f.debug_tuple("Text").field(&s.as_str()).finish(),
            XmlEvent::CData(s) => f.debug_tuple("CData").field(s).finish(),
            XmlEvent::Comment(s) => f.debug_tuple("Comment").field(s).finish(),
            XmlEvent::ProcessingInstruction { target, data } => f
                .debug_struct("ProcessingInstruction")
                .field("target", target)
                .field("data", data)
                .finish(),
        }
    }
}

/// Escape the 5 XML 1.0 special characters (`&`, `<`, `>`, `"`, `'`) into `out`.
pub fn escape_xml_into<W: fmt::Write>(input: &str, out: &mut W) -> fmt::Result {
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&apos;",
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(chunk) = input.get(start..i)
            && !chunk.is_empty()
        {
            out.write_str(chunk)?;
        }
        out.write_str(replacement)?;
        i += 1;
        start = i;
    }
    if let Some(tail) = input.get(start..)
        && !tail.is_empty()
    {
        out.write_str(tail)?;
    }
    Ok(())
}

/// Escape the 5 XML 1.0 special characters (`&`, `<`, `>`, `"`, `'`) into a new [`String`].
pub fn escape_xml_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut buf = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { buf: &mut buf, err: None };
    if escape_xml_into(input, &mut writer).is_err() {
        let len = writer.buf.len();
        return Err(writer.err.unwrap_or(AllocFailure::CapacityOverflow {
            what: "bstd::xml::escape_xml_in",
            len,
            additional: 1,
        }));
    }
    Ok(buf)
}

struct TryStringWriter<'a, A: Allocator> {
    buf: &'a mut String<A>,
    err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self.buf.try_push_str(s) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.err = Some(e);
                Err(fmt::Error)
            }
        }
    }
}

/// Iterative, bounded, XXE-proof XML 1.0 pull parser.
pub struct XmlReader<'a, A: Allocator> {
    src: &'a str,
    pos: usize,
    limits: XmlLimits,
    alloc: A,
    stack: Vec<&'a str, A>,
    events_emitted: usize,
    #[cfg(test)]
    plant_skip_end_tag_check: bool,
}

impl<A: Allocator> fmt::Debug for XmlReader<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XmlReader")
            .field("pos", &self.pos)
            .field("len", &self.src.len())
            .field("depth", &self.stack.len())
            .field("events_emitted", &self.events_emitted)
            .field("limits", &self.limits)
            .finish()
    }
}

impl<'a, A: Allocator + Copy> XmlReader<'a, A> {
    /// Create a pull parser over `src` with explicit `limits` and allocator `alloc`.
    pub fn new_in(src: &'a str, limits: XmlLimits, alloc: A) -> Result<Self, XmlError> {
        Self::new_in_inner(src, limits, alloc, false)
    }

    fn new_in_inner(
        src: &'a str,
        limits: XmlLimits,
        alloc: A,
        plant_skip_end_tag_check: bool,
    ) -> Result<Self, XmlError> {
        #[cfg(not(test))]
        let _ = plant_skip_end_tag_check;
        if src.len() > limits.most_bytes {
            return Err(XmlError::InputTooLarge { bytes: src.len(), limit: limits.most_bytes });
        }
        let pos = if src.starts_with('\u{FEFF}') { '\u{FEFF}'.len_utf8() } else { 0 };
        Ok(Self {
            src,
            pos,
            limits,
            alloc,
            stack: Vec::new_in(alloc),
            events_emitted: 0,
            #[cfg(test)]
            plant_skip_end_tag_check,
        })
    }

    /// Current element nesting depth.
    #[inline]
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Current byte offset within the input slice.
    #[inline]
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.pos
    }

    /// Number of events emitted so far.
    #[inline]
    #[must_use]
    pub const fn events_emitted(&self) -> usize {
        self.events_emitted
    }

    /// Pull the next [`XmlEvent`], returning `Ok(None)` at EOF after verifying all opened elements
    /// were closed.
    pub fn next_event(&mut self) -> Result<Option<XmlEvent<'a, A>>, XmlError> {
        loop {
            let Some(rest) = self.src.get(self.pos..) else {
                return self.finish_eof();
            };
            if rest.is_empty() {
                return self.finish_eof();
            }

            let start = self.pos;
            let first = match rest.as_bytes().first() {
                Some(&b) => b,
                None => return self.finish_eof(),
            };

            if first != b'<' {
                return self.read_text(rest, start).map(Some);
            }

            if rest.starts_with("<!--") {
                return self.read_comment(rest, start).map(Some);
            }
            if rest.starts_with("<![CDATA[") {
                return self.read_cdata(rest, start).map(Some);
            }
            if starts_with_entity_decl(rest) {
                return Err(XmlError::EntityDeclForbidden);
            }
            if starts_with_doctype(rest) {
                self.skip_doctype(start)?;
                continue;
            }
            if rest.starts_with("<!") {
                return Err(XmlError::UnexpectedSyntax { offset: start });
            }
            if rest.starts_with("<?") {
                return self.read_pi(rest, start).map(Some);
            }
            if rest.starts_with("</") {
                return self.read_end_element(start).map(Some);
            }
            return self.read_start_element(start).map(Some);
        }
    }

    fn finish_eof(&self) -> Result<Option<XmlEvent<'a, A>>, XmlError> {
        if !self.stack.is_empty() {
            return Err(XmlError::UnclosedElement { depth: self.stack.len(), offset: self.pos });
        }
        Ok(None)
    }

    fn count_event(&mut self, offset: usize) -> Result<(), XmlError> {
        if self.events_emitted >= self.limits.most_events {
            return Err(XmlError::TooManyEvents { limit: self.limits.most_events, offset });
        }
        self.events_emitted += 1;
        Ok(())
    }

    fn read_text(&mut self, rest: &'a str, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let rel_end = rest.as_bytes().iter().position(|&b| b == b'<').unwrap_or(rest.len());
        let raw = rest.get(..rel_end).ok_or(XmlError::UnexpectedSyntax { offset: start })?;
        if raw.contains("]]>") {
            return Err(XmlError::UnexpectedSyntax { offset: start });
        }
        self.count_event(start)?;
        let decoded = unescape_xml_in(raw, start, false, self.alloc)?;
        self.pos = start + rel_end;
        Ok(XmlEvent::Text(decoded))
    }

    fn read_comment(&mut self, rest: &'a str, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let after_open = rest.get(4..).ok_or(XmlError::UnexpectedEof { offset: start })?;
        let rel_end = after_open.find("-->").ok_or(XmlError::UnexpectedEof { offset: start })?;
        let body = after_open.get(..rel_end).ok_or(XmlError::UnexpectedSyntax { offset: start })?;
        if body.contains("--") || body.ends_with('-') || !all_valid_xml_chars(body) {
            return Err(XmlError::InvalidComment { offset: start });
        }
        self.count_event(start)?;
        self.pos = start + 4 + rel_end + 3;
        Ok(XmlEvent::Comment(body))
    }

    fn read_cdata(&mut self, rest: &'a str, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let after_open = rest.get(9..).ok_or(XmlError::UnexpectedEof { offset: start })?;
        let rel_end = after_open.find("]]>").ok_or(XmlError::UnexpectedEof { offset: start })?;
        let body = after_open.get(..rel_end).ok_or(XmlError::UnexpectedSyntax { offset: start })?;
        if !all_valid_xml_chars(body) {
            return Err(XmlError::InvalidCData { offset: start });
        }
        self.count_event(start)?;
        self.pos = start + 9 + rel_end + 3;
        Ok(XmlEvent::CData(body))
    }

    fn read_pi(&mut self, rest: &'a str, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let after_open = rest.get(2..).ok_or(XmlError::UnexpectedEof { offset: start })?;
        let target_len = scan_xml_name(after_open);
        if target_len == 0 {
            return Err(XmlError::InvalidPi { offset: start });
        }
        let target = after_open
            .get(..target_len)
            .ok_or(XmlError::InvalidPi { offset: start })?;
        let tail = after_open
            .get(target_len..)
            .ok_or(XmlError::UnexpectedEof { offset: start })?;
        let rel_end = tail.find("?>").ok_or(XmlError::UnexpectedEof { offset: start })?;
        let raw_data = tail.get(..rel_end).ok_or(XmlError::InvalidPi { offset: start })?;
        let data = if raw_data.is_empty() {
            ""
        } else {
            match raw_data.as_bytes().first() {
                Some(&b) if is_xml_whitespace(b) => trim_leading_xml_ws(raw_data),
                Some(_) | None => return Err(XmlError::InvalidPi { offset: start + 2 + target_len }),
            }
        };
        if !all_valid_xml_chars(data) {
            return Err(XmlError::InvalidPi { offset: start });
        }
        self.count_event(start)?;
        self.pos = start + 2 + target_len + rel_end + 2;
        Ok(XmlEvent::ProcessingInstruction { target, data })
    }

    fn skip_doctype(&mut self, start: usize) -> Result<(), XmlError> {
        let bytes = self.src.as_bytes();
        let after_kw = start + 9;
        match bytes.get(after_kw) {
            Some(&b) if is_xml_whitespace(b) => {}
            Some(_) | None => return Err(XmlError::InvalidDoctype { offset: start }),
        }

        let mut cur = after_kw + 1;
        let mut in_quote: Option<u8> = None;
        let mut in_subset = false;
        let mut in_decl = false;

        while let Some(&b) = bytes.get(cur) {
            if let Some(q) = in_quote {
                if b == q {
                    in_quote = None;
                }
                cur += 1;
                continue;
            }
            match b {
                b'"' | b'\'' => {
                    in_quote = Some(b);
                    cur += 1;
                }
                b'[' if !in_subset && !in_decl => {
                    in_subset = true;
                    cur += 1;
                }
                b']' if in_subset && !in_decl => {
                    in_subset = false;
                    cur += 1;
                }
                b'>' => {
                    if in_subset {
                        if in_decl {
                            in_decl = false;
                            cur += 1;
                        } else {
                            return Err(XmlError::InvalidDoctype { offset: cur });
                        }
                    } else {
                        self.pos = cur + 1;
                        return Ok(());
                    }
                }
                b'<' if in_subset && !in_decl => {
                    let sub = self
                        .src
                        .get(cur..)
                        .ok_or(XmlError::UnexpectedEof { offset: cur })?;
                    if sub.starts_with("<!--") {
                        let body = sub
                            .get(4..)
                            .ok_or(XmlError::UnexpectedEof { offset: cur })?;
                        let rel = body
                            .find("-->")
                            .ok_or(XmlError::UnexpectedEof { offset: cur })?;
                        cur += 4 + rel + 3;
                    } else if sub.starts_with("<?") {
                        let body = sub
                            .get(2..)
                            .ok_or(XmlError::UnexpectedEof { offset: cur })?;
                        let rel = body
                            .find("?>")
                            .ok_or(XmlError::UnexpectedEof { offset: cur })?;
                        cur += 2 + rel + 2;
                    } else if starts_with_entity_decl(sub) {
                        return Err(XmlError::EntityDeclForbidden);
                    } else if sub.starts_with("<!") {
                        in_decl = true;
                        cur += 2;
                    } else {
                        return Err(XmlError::InvalidDoctype { offset: cur });
                    }
                }
                b'<' => return Err(XmlError::InvalidDoctype { offset: cur }),
                _ => cur += 1,
            }
        }
        Err(XmlError::UnexpectedEof { offset: start })
    }

    fn read_end_element(&mut self, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let mut cur = start + 2;
        let name_rest = self
            .src
            .get(cur..)
            .ok_or(XmlError::UnexpectedEof { offset: start })?;
        let name_len = scan_xml_name(name_rest);
        if name_len == 0 {
            return Err(XmlError::InvalidName { offset: cur });
        }
        let name = name_rest
            .get(..name_len)
            .ok_or(XmlError::InvalidName { offset: cur })?;
        cur += name_len;
        cur = skip_xml_ws(self.src.as_bytes(), cur);
        if self.src.as_bytes().get(cur) != Some(&b'>') {
            return Err(XmlError::UnexpectedSyntax { offset: cur });
        }
        cur += 1;

        let expected = match self.stack.pop() {
            Some(tag) => tag,
            None => return Err(XmlError::UnexpectedEndTag { offset: start }),
        };
        #[cfg(test)]
        let check_match = !self.plant_skip_end_tag_check;
        #[cfg(not(test))]
        let check_match = true;
        if check_match && expected != name {
            return Err(XmlError::MismatchedEndTag { offset: start });
        }

        self.count_event(start)?;
        self.pos = cur;
        Ok(XmlEvent::EndElement { name })
    }

    fn read_start_element(&mut self, start: usize) -> Result<XmlEvent<'a, A>, XmlError> {
        let bytes = self.src.as_bytes();
        let mut cur = start + 1;
        let name_rest = self
            .src
            .get(cur..)
            .ok_or(XmlError::UnexpectedEof { offset: start })?;
        let name_len = scan_xml_name(name_rest);
        if name_len == 0 {
            return Err(XmlError::InvalidName { offset: cur });
        }
        let name = name_rest
            .get(..name_len)
            .ok_or(XmlError::InvalidName { offset: cur })?;
        cur += name_len;

        let mut attrs = Vec::<XmlAttr<'a, A>, A>::new_in(self.alloc);
        let empty: bool;

        loop {
            let ws_start = cur;
            cur = skip_xml_ws(bytes, cur);
            let had_ws = cur > ws_start;

            match bytes.get(cur) {
                None => return Err(XmlError::UnexpectedEof { offset: start }),
                Some(&b'>') => {
                    cur += 1;
                    empty = false;
                    break;
                }
                Some(&b'/') => {
                    if bytes.get(cur + 1) != Some(&b'>') {
                        return Err(XmlError::UnexpectedSyntax { offset: cur });
                    }
                    cur += 2;
                    empty = true;
                    break;
                }
                Some(_) => {
                    if !had_ws {
                        return Err(XmlError::InvalidAttr { offset: cur });
                    }
                    if attrs.len() >= self.limits.most_attrs {
                        return Err(XmlError::TooManyAttrs {
                            limit: self.limits.most_attrs,
                            offset: cur,
                        });
                    }
                    let attr_start = cur;
                    let attr_rest = self
                        .src
                        .get(cur..)
                        .ok_or(XmlError::UnexpectedEof { offset: attr_start })?;
                    let attr_name_len = scan_xml_name(attr_rest);
                    if attr_name_len == 0 {
                        return Err(XmlError::InvalidName { offset: attr_start });
                    }
                    let attr_name = attr_rest
                        .get(..attr_name_len)
                        .ok_or(XmlError::InvalidName { offset: attr_start })?;
                    if attrs.as_slice().iter().any(|existing| existing.name == attr_name) {
                        return Err(XmlError::DuplicateAttr { offset: attr_start });
                    }
                    cur += attr_name_len;
                    cur = skip_xml_ws(bytes, cur);
                    if bytes.get(cur) != Some(&b'=') {
                        return Err(XmlError::InvalidAttr { offset: cur });
                    }
                    cur += 1;
                    cur = skip_xml_ws(bytes, cur);
                    let quote = match bytes.get(cur) {
                        Some(&b'"') => b'"',
                        Some(&b'\'') => b'\'',
                        Some(_) => return Err(XmlError::InvalidAttr { offset: cur }),
                        None => return Err(XmlError::UnexpectedEof { offset: attr_start }),
                    };
                    cur += 1;
                    let val_start = cur;
                    let mut val_end = None;
                    while let Some(&b) = bytes.get(cur) {
                        if b == quote {
                            val_end = Some(cur);
                            break;
                        }
                        if b == b'<' {
                            return Err(XmlError::InvalidAttr { offset: cur });
                        }
                        cur += 1;
                    }
                    let end_quote =
                        val_end.ok_or(XmlError::UnexpectedEof { offset: attr_start })?;
                    let raw_val = self
                        .src
                        .get(val_start..end_quote)
                        .ok_or(XmlError::InvalidAttr { offset: val_start })?;
                    let value = unescape_xml_in(raw_val, val_start, true, self.alloc)?;
                    cur = end_quote + 1;
                    attrs
                        .try_push(XmlAttr { name: attr_name, value })
                        .map_err(|r| XmlError::Alloc(r.why))?;
                }
            }
        }

        if self.stack.len() >= self.limits.most_depth {
            return Err(XmlError::DepthLimitExceeded {
                limit: self.limits.most_depth,
                offset: start,
            });
        }
        self.count_event(start)?;
        if !empty {
            self.stack.try_push(name).map_err(|r| XmlError::Alloc(r.why))?;
        }
        self.pos = cur;
        Ok(XmlEvent::StartElement { name, attrs, empty })
    }
}

/// Canonical, iterative XML 1.0 writer with an explicit open-element stack.
pub struct XmlWriter<A: Allocator> {
    buf: String<A>,
    stack: Vec<String<A>, A>,
    alloc: A,
}

impl<A: Allocator> fmt::Debug for XmlWriter<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XmlWriter")
            .field("bytes", &self.buf.len())
            .field("depth", &self.stack.len())
            .finish()
    }
}

impl<A: Allocator + Copy> XmlWriter<A> {
    /// Construct a new empty writer backed by `alloc`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { buf: String::new_in(alloc), stack: Vec::new_in(alloc), alloc }
    }

    /// Current open-element nesting depth.
    #[inline]
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Borrow the XML bytes written so far.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.buf.as_str()
    }

    /// Write the canonical XML 1.0 UTF-8 declaration (`<?xml version="1.0" encoding="UTF-8"?>`).
    pub fn write_decl(&mut self) -> Result<(), XmlError> {
        self.buf
            .try_push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>")
            .map_err(XmlError::Alloc)
    }

    /// Write an opening element `<name k="v"...>` and push `name` onto the open-element stack.
    pub fn start_element(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), XmlError> {
        validate_element_head(name, attrs, self.buf.len())?;
        let owned_name = String::try_from_str_in(name, self.alloc).map_err(XmlError::Alloc)?;
        self.stack.try_reserve(1).map_err(XmlError::Alloc)?;
        self.write_open_tag(name, attrs, false)?;
        self.stack.try_push(owned_name).map_err(|r| XmlError::Alloc(r.why))
    }

    /// Write a self-closing empty element `<name k="v".../>`.
    pub fn empty_element(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), XmlError> {
        validate_element_head(name, attrs, self.buf.len())?;
        self.write_open_tag(name, attrs, true)
    }

    /// Write escaped XML text content.
    pub fn text(&mut self, text: &str) -> Result<(), XmlError> {
        if !all_valid_xml_chars(text) {
            return Err(XmlError::UnexpectedSyntax { offset: self.buf.len() });
        }
        escape_text_into_string(text, false, &mut self.buf).map_err(XmlError::Alloc)
    }

    /// Write a verbatim `<![CDATA[...]]>` section, rejecting payloads containing `]]>`.
    pub fn cdata(&mut self, data: &str) -> Result<(), XmlError> {
        if data.contains("]]>") || !all_valid_xml_chars(data) {
            return Err(XmlError::InvalidCData { offset: self.buf.len() });
        }
        self.buf.try_push_str("<![CDATA[").map_err(XmlError::Alloc)?;
        self.buf.try_push_str(data).map_err(XmlError::Alloc)?;
        self.buf.try_push_str("]]>").map_err(XmlError::Alloc)
    }

    /// Write an XML comment `<!--...-->`, rejecting `--` inside or trailing `-`.
    pub fn comment(&mut self, text: &str) -> Result<(), XmlError> {
        if text.contains("--") || text.ends_with('-') || !all_valid_xml_chars(text) {
            return Err(XmlError::InvalidComment { offset: self.buf.len() });
        }
        self.buf.try_push_str("<!--").map_err(XmlError::Alloc)?;
        self.buf.try_push_str(text).map_err(XmlError::Alloc)?;
        self.buf.try_push_str("-->").map_err(XmlError::Alloc)
    }

    /// Pop the most recently opened element and write its closing `</name>` tag.
    pub fn end_element(&mut self) -> Result<(), XmlError> {
        let name = match self.stack.pop() {
            Some(tag) => tag,
            None => return Err(XmlError::UnexpectedEndTag { offset: self.buf.len() }),
        };
        self.buf.try_push_str("</").map_err(XmlError::Alloc)?;
        self.buf.try_push_str(name.as_str()).map_err(XmlError::Alloc)?;
        self.buf.try_push('>').map_err(XmlError::Alloc)
    }

    /// Finalize and return the XML document, refusing any unclosed elements.
    pub fn finish(self) -> Result<String<A>, XmlError> {
        if !self.stack.is_empty() {
            return Err(XmlError::UnclosedElement {
                depth: self.stack.len(),
                offset: self.buf.len(),
            });
        }
        Ok(self.buf)
    }

    fn write_open_tag(
        &mut self,
        name: &str,
        attrs: &[(&str, &str)],
        empty: bool,
    ) -> Result<(), XmlError> {
        self.buf.try_push('<').map_err(XmlError::Alloc)?;
        self.buf.try_push_str(name).map_err(XmlError::Alloc)?;
        for &(key, val) in attrs {
            self.buf.try_push(' ').map_err(XmlError::Alloc)?;
            self.buf.try_push_str(key).map_err(XmlError::Alloc)?;
            self.buf.try_push_str("=\"").map_err(XmlError::Alloc)?;
            escape_text_into_string(val, true, &mut self.buf).map_err(XmlError::Alloc)?;
            self.buf.try_push('"').map_err(XmlError::Alloc)?;
        }
        if empty {
            self.buf.try_push_str("/>").map_err(XmlError::Alloc)
        } else {
            self.buf.try_push('>').map_err(XmlError::Alloc)
        }
    }
}

fn validate_element_head(name: &str, attrs: &[(&str, &str)], offset: usize) -> Result<(), XmlError> {
    if !is_valid_xml_name(name) {
        return Err(XmlError::InvalidName { offset });
    }
    for (idx, &(key, val)) in attrs.iter().enumerate() {
        if !is_valid_xml_name(key) {
            return Err(XmlError::InvalidName { offset });
        }
        if let Some(prev) = attrs.get(..idx)
            && prev.iter().any(|&(seen, _)| seen == key)
        {
            return Err(XmlError::DuplicateAttr { offset });
        }
        if !all_valid_xml_chars(val) {
            return Err(XmlError::InvalidAttr { offset });
        }
    }
    Ok(())
}

fn escape_text_into_string<A: Allocator>(
    input: &str,
    for_attr: bool,
    out: &mut String<A>,
) -> Result<(), AllocFailure> {
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&apos;",
            b'\r' => "&#13;",
            b'\n' if for_attr => "&#10;",
            b'\t' if for_attr => "&#9;",
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(chunk) = input.get(start..i)
            && !chunk.is_empty()
        {
            out.try_push_str(chunk)?;
        }
        out.try_push_str(replacement)?;
        i += 1;
        start = i;
    }
    if let Some(tail) = input.get(start..)
        && !tail.is_empty()
    {
        out.try_push_str(tail)?;
    }
    Ok(())
}

fn unescape_xml_in<A: Allocator + Copy>(
    raw: &str,
    base_offset: usize,
    normalize_attr_ws: bool,
    alloc: A,
) -> Result<String<A>, XmlError> {
    let mut out = String::try_with_capacity_in(raw.len(), alloc).map_err(XmlError::Alloc)?;
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;

    while let Some(&b) = bytes.get(i) {
        if b == b'&' {
            if let Some(chunk) = raw.get(chunk_start..i)
                && !chunk.is_empty()
            {
                if !all_valid_xml_chars(chunk) {
                    return Err(XmlError::UnexpectedSyntax { offset: base_offset + chunk_start });
                }
                out.try_push_str(chunk).map_err(XmlError::Alloc)?;
            }
            let tail = raw
                .get(i..)
                .ok_or(XmlError::InvalidEntity { offset: base_offset + i })?;
            let (ch, consumed) = decode_xml_entity(tail, base_offset + i)?;
            out.try_push(ch).map_err(XmlError::Alloc)?;
            i += consumed;
            chunk_start = i;
            continue;
        }
        if b == b'\r' {
            if let Some(chunk) = raw.get(chunk_start..i)
                && !chunk.is_empty()
            {
                if !all_valid_xml_chars(chunk) {
                    return Err(XmlError::UnexpectedSyntax { offset: base_offset + chunk_start });
                }
                out.try_push_str(chunk).map_err(XmlError::Alloc)?;
            }
            let replacement = if normalize_attr_ws { ' ' } else { '\n' };
            out.try_push(replacement).map_err(XmlError::Alloc)?;
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
            chunk_start = i;
            continue;
        }
        if normalize_attr_ws && (b == b'\n' || b == b'\t') {
            if let Some(chunk) = raw.get(chunk_start..i)
                && !chunk.is_empty()
            {
                if !all_valid_xml_chars(chunk) {
                    return Err(XmlError::UnexpectedSyntax { offset: base_offset + chunk_start });
                }
                out.try_push_str(chunk).map_err(XmlError::Alloc)?;
            }
            out.try_push(' ').map_err(XmlError::Alloc)?;
            i += 1;
            chunk_start = i;
            continue;
        }
        i += 1;
    }

    if let Some(tail) = raw.get(chunk_start..)
        && !tail.is_empty()
    {
        if !all_valid_xml_chars(tail) {
            return Err(XmlError::UnexpectedSyntax { offset: base_offset + chunk_start });
        }
        out.try_push_str(tail).map_err(XmlError::Alloc)?;
    }

    Ok(out)
}

fn decode_xml_entity(rest: &str, offset: usize) -> Result<(char, usize), XmlError> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'&') {
        return Err(XmlError::InvalidEntity { offset });
    }
    let mut idx = 1usize;
    let mut semi_pos = None;
    while let Some(&b) = bytes.get(idx) {
        if b == b';' {
            semi_pos = Some(idx);
            break;
        }
        if matches!(b, b'&' | b'<' | b' ' | b'\t' | b'\r' | b'\n') {
            return Err(XmlError::InvalidEntity { offset });
        }
        idx += 1;
    }
    let semi = semi_pos.ok_or(XmlError::InvalidEntity { offset })?;
    let body = rest.get(1..semi).ok_or(XmlError::InvalidEntity { offset })?;
    if body.is_empty() {
        return Err(XmlError::InvalidEntity { offset });
    }
    let consumed = semi + 1;

    if let Some(num) = body.strip_prefix('#') {
        let cp = if let Some(hex) = num.strip_prefix('x') {
            parse_hex_scalar(hex).ok_or(XmlError::InvalidEntity { offset })?
        } else {
            parse_dec_scalar(num).ok_or(XmlError::InvalidEntity { offset })?
        };
        let ch = char::from_u32(cp).ok_or(XmlError::InvalidEntity { offset })?;
        if !is_valid_xml_char(ch) {
            return Err(XmlError::InvalidEntity { offset });
        }
        return Ok((ch, consumed));
    }

    let ch = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return Err(XmlError::UnknownEntity { offset }),
    };
    Ok((ch, consumed))
}

fn parse_dec_scalar(digits: &str) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for &b in digits.as_bytes() {
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            _ => return None,
        };
        acc = acc.saturating_mul(10).saturating_add(d);
        if acc > 0x10_FFFF {
            return None;
        }
    }
    Some(acc)
}

fn parse_hex_scalar(digits: &str) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for &b in digits.as_bytes() {
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' => u32::from(b - b'a') + 10,
            b'A'..=b'F' => u32::from(b - b'A') + 10,
            _ => return None,
        };
        acc = acc.saturating_mul(16).saturating_add(d);
        if acc > 0x10_FFFF {
            return None;
        }
    }
    Some(acc)
}

#[inline]
fn starts_with_entity_decl(rest: &str) -> bool {
    rest.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("<!ENTITY"))
}

#[inline]
fn starts_with_doctype(rest: &str) -> bool {
    rest.starts_with("<!DOCTYPE")
}

#[inline]
const fn is_xml_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_xml_ws(bytes: &[u8], mut cur: usize) -> usize {
    while let Some(&b) = bytes.get(cur) {
        if is_xml_whitespace(b) {
            cur += 1;
        } else {
            break;
        }
    }
    cur
}

fn trim_leading_xml_ws(s: &str) -> &str {
    let start = skip_xml_ws(s.as_bytes(), 0);
    s.get(start..).unwrap_or("")
}

/// XML 1.0 §2.2 valid character check.
#[inline]
fn is_valid_xml_char(c: char) -> bool {
    let cp = u32::from(c);
    matches!(cp, 0x09 | 0x0A | 0x0D | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

fn all_valid_xml_chars(s: &str) -> bool {
    s.chars().all(is_valid_xml_char)
}

/// XML 1.0 (5th ed.) §2.3 `NameStartChar`.
fn is_xml_name_start_char(c: char) -> bool {
    let cp = u32::from(c);
    matches!(
        cp,
        0x3A
            | 0x41..=0x5A
            | 0x5F
            | 0x61..=0x7A
            | 0xC0..=0xD6
            | 0xD8..=0xF6
            | 0xF8..=0x2FF
            | 0x370..=0x37D
            | 0x37F..=0x1FFF
            | 0x200C..=0x200D
            | 0x2070..=0x218F
            | 0x2C00..=0x2FEF
            | 0x3001..=0xD7FF
            | 0xF900..=0xFDCF
            | 0xFDF0..=0xFFFD
            | 0x10000..=0xEFFFF
    )
}

/// XML 1.0 (5th ed.) §2.3 `NameChar`.
fn is_xml_name_char(c: char) -> bool {
    if is_xml_name_start_char(c) {
        return true;
    }
    let cp = u32::from(c);
    matches!(cp, 0x2D | 0x2E | 0x30..=0x39 | 0xB7 | 0x0300..=0x036F | 0x203F..=0x2040)
}

fn scan_xml_name(s: &str) -> usize {
    let mut chars = s.char_indices();
    let Some((_, first)) = chars.next() else {
        return 0;
    };
    if !is_xml_name_start_char(first) {
        return 0;
    }
    for (idx, ch) in chars {
        if !is_xml_name_char(ch) {
            return idx;
        }
    }
    s.len()
}

fn is_valid_xml_name(s: &str) -> bool {
    !s.is_empty() && scan_xml_name(s) == s.len()
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::*;

    fn collect_events<'a, A: Allocator + Copy>(
        src: &'a str,
        limits: XmlLimits,
        alloc: A,
    ) -> Result<OwnVec<XmlEvent<'a, A>, A>, XmlError> {
        let mut reader = XmlReader::new_in(src, limits, alloc)?;
        let mut out = OwnVec::new_in(alloc);
        while let Some(ev) = reader.next_event()? {
            out.push(ev);
        }
        Ok(out)
    }

    #[test]
    fn xml_decl_elements_attrs_cdata_comments_and_entities() {
        let h = heap();
        let doc = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                   <!-- lead comment -->\
                   <root id=\"a&amp;b\" title='it&apos;s &lt;ok&gt; &#65;&#x42;'>\
                   hello &quot;world&quot;\
                   <empty k=\"1\"/>\
                   <![CDATA[raw <tag> &amp; text]]>\
                   </root>";

        let events = collect_events(doc, XmlLimits::SHIPPED, h.alloc()).expect("parse xml");
        assert_eq!(events.len(), 7);

        assert_eq!(
            events.first(),
            Some(&XmlEvent::ProcessingInstruction {
                target: "xml",
                data: "version=\"1.0\" encoding=\"UTF-8\"",
            })
        );
        assert_eq!(events.get(1), Some(&XmlEvent::Comment(" lead comment ")));

        let mut expected_root_attrs = OwnVec::new_in(h.alloc());
        expected_root_attrs.push(XmlAttr {
            name: "id",
            value: OwnString::from_str_in("a&b", h.alloc()),
        });
        expected_root_attrs.push(XmlAttr {
            name: "title",
            value: OwnString::from_str_in("it's <ok> AB", h.alloc()),
        });
        assert_eq!(
            events.get(2),
            Some(&XmlEvent::StartElement {
                name: "root",
                attrs: expected_root_attrs,
                empty: false,
            })
        );
        assert_eq!(
            events.get(3),
            Some(&XmlEvent::Text(OwnString::from_str_in("hello \"world\"", h.alloc())))
        );

        let mut expected_empty_attrs = OwnVec::new_in(h.alloc());
        expected_empty_attrs.push(XmlAttr {
            name: "k",
            value: OwnString::from_str_in("1", h.alloc()),
        });
        assert_eq!(
            events.get(4),
            Some(&XmlEvent::StartElement {
                name: "empty",
                attrs: expected_empty_attrs,
                empty: true,
            })
        );
        assert_eq!(events.get(5), Some(&XmlEvent::CData("raw <tag> &amp; text")));
        assert_eq!(events.get(6), Some(&XmlEvent::EndElement { name: "root" }));
    }

    #[test]
    fn xml_security_xxe_limits_and_well_formedness_errors() {
        let h = heap();

        // XXE & Billion Laughs declarations are explicitly refused.
        let xxe_doctype =
            "<!DOCTYPE foo [ <!ENTITY xxe SYSTEM \"file:///etc/passwd\"> ]><foo>&xxe;</foo>";
        assert_eq!(
            collect_events(xxe_doctype, XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::EntityDeclForbidden)
        );
        let top_entity = "<!ENTITY lol \"lol\"><a/>";
        assert_eq!(
            collect_events(top_entity, XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::EntityDeclForbidden)
        );

        // Unknown named entity refused.
        assert!(matches!(
            collect_events("<a>&nbsp;</a>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::UnknownEntity { .. })
        ));

        // Mismatched and unclosed tags refused.
        assert!(matches!(
            collect_events("<a><b></a></b>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::MismatchedEndTag { .. })
        ));
        assert!(matches!(
            collect_events("<a><b></b>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::UnclosedElement { depth: 1, .. })
        ));
        assert!(matches!(
            collect_events("</a>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::UnexpectedEndTag { .. })
        ));

        // Comment containing `--` refused per XML 1.0 §2.5.
        assert!(matches!(
            collect_events("<!-- bad -- comment --><a/>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::InvalidComment { .. })
        ));
        assert!(matches!(
            collect_events("<!-- trailing ---><a/>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::InvalidComment { .. })
        ));

        // Duplicate attribute refused.
        assert!(matches!(
            collect_events("<a x=\"1\" x=\"2\"/>", XmlLimits::SHIPPED, h.alloc()),
            Err(XmlError::DuplicateAttr { .. })
        ));

        // Depth limit refused.
        let shallow = XmlLimits { most_bytes: 1024, most_depth: 2, most_attrs: 8, most_events: 64 };
        assert!(collect_events("<a><b/></a>", shallow, h.alloc()).is_ok());
        assert!(matches!(
            collect_events("<a><b><c/></b></a>", shallow, h.alloc()),
            Err(XmlError::DepthLimitExceeded { limit: 2, .. })
        ));

        // Safe DOCTYPE without <!ENTITY is skipped cleanly.
        let safe_dt = "<!DOCTYPE note><note>ok</note>";
        let evs = collect_events(safe_dt, XmlLimits::SHIPPED, h.alloc()).expect("safe doctype");
        assert_eq!(evs.len(), 3);
    }

    #[test]
    fn xml_writer_validation_and_canonical_output() {
        let h = heap();
        let mut w = XmlWriter::new_in(h.alloc());
        w.write_decl().expect("decl");
        w.start_element("doc", &[("k", "a&b\"c'd<e>f")]).expect("start");
        w.text("x < y & z > 0").expect("text");
        w.empty_element("br", &[]).expect("empty");
        w.cdata("verbatim <ok>").expect("cdata");
        w.comment(" safe note ").expect("comment");

        // Reject invalid CDATA, comments, names, duplicates, and unclosed finish.
        assert!(matches!(w.cdata("bad ]]> end"), Err(XmlError::InvalidCData { .. })));
        assert!(matches!(w.comment("bad -- comment"), Err(XmlError::InvalidComment { .. })));
        assert!(matches!(w.comment("ends-"), Err(XmlError::InvalidComment { .. })));
        assert!(matches!(w.start_element("1bad", &[]), Err(XmlError::InvalidName { .. })));
        assert!(matches!(
            w.empty_element("e", &[("a", "1"), ("a", "2")]),
            Err(XmlError::DuplicateAttr { .. })
        ));

        w.end_element().expect("end");
        assert!(matches!(w.end_element(), Err(XmlError::UnexpectedEndTag { .. })));

        let xml = w.finish().expect("finish");
        assert_eq!(
            xml.as_str(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <doc k=\"a&amp;b&quot;c&apos;d&lt;e&gt;f\">\
             x &lt; y &amp; z &gt; 0\
             <br/>\
             <![CDATA[verbatim <ok>]]>\
             <!-- safe note -->\
             </doc>"
        );

        let mut unclosed = XmlWriter::new_in(h.alloc());
        unclosed.start_element("open", &[]).expect("open");
        assert!(matches!(unclosed.finish(), Err(XmlError::UnclosedElement { depth: 1, .. })));
    }

    enum ExpectedEvent<A: Allocator> {
        Start { name: &'static str, attrs: OwnVec<(&'static str, OwnString<A>), A>, empty: bool },
        End { name: &'static str },
        Text(OwnString<A>),
        CData(&'static str),
        Comment(&'static str),
        Pi { target: &'static str, data: &'static str },
    }

    const TAG_NAMES: &[&str] = &["root", "item", "ns:node", "entry_1", "leaf-x", "café"];
    const ATTR_NAMES: &[&str] = &["id", "class", "xml:lang", "data-v", "k_1"];
    const TEXT_PIECES: &[&str] = &[
        "plain",
        "&",
        "<",
        ">",
        "\"",
        "'",
        "a & b < c",
        "]]>",
        "\n\t",
        "\r\n",
        "日本語",
        "🦀",
    ];
    const CDATA_SAMPLES: &[&str] = &["raw <xml> &amp;", "1 < 2 && 3 > 1", "line1\nline2"];
    const COMMENT_SAMPLES: &[&str] = &[" note ", "a - b", "xml comment 123"];

    fn draw_text_value<A: Allocator>(tape: &Tape, alloc: A) -> OwnString<A> {
        let pieces = tape.int_in(1usize..=4, "pieces");
        let mut s = OwnString::new_in(alloc);
        for _ in 0..pieces {
            let idx = tape.index(TEXT_PIECES.len(), "piece");
            if let Some(&p) = TEXT_PIECES.get(idx) {
                s.push_str(p);
            }
        }
        s
    }

    fn build_and_verify_random_tree<A: Allocator + Copy>(
        tape: &Tape,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut writer = XmlWriter::new_in(alloc);
        let mut expected = OwnVec::<ExpectedEvent<A>, A>::new_in(alloc);
        let mut open_tags = OwnVec::<&'static str, A>::new_in(alloc);

        if tape.coin("decl") {
            writer
                .write_decl()
                .map_err(|e| Broke::Harness(format_in!(alloc, "write_decl: {e}").as_str().into()))?;
            expected.push(ExpectedEvent::Pi {
                target: "xml",
                data: "version=\"1.0\" encoding=\"UTF-8\"",
            });
        }

        let root_name = TAG_NAMES.get(tape.index(TAG_NAMES.len(), "root")).copied().unwrap_or("root");
        writer
            .start_element(root_name, &[])
            .map_err(|e| Broke::Harness(format_in!(alloc, "start root: {e}").as_str().into()))?;
        expected.push(ExpectedEvent::Start {
            name: root_name,
            attrs: OwnVec::new_in(alloc),
            empty: false,
        });
        open_tags.push(root_name);

        let ops = tape.int_in(0usize..=24, "ops");
        let mut last_was_text = false;

        for _ in 0..ops {
            let kind = tape.index(6, "op_kind");
            match kind {
                // Nested start element
                0 if open_tags.len() < 6 => {
                    let name = TAG_NAMES
                        .get(tape.index(TAG_NAMES.len(), "tag"))
                        .copied()
                        .unwrap_or("item");
                    let attr_count = tape.int_in(0usize..=3, "attrs");
                    let mut owned_attrs = OwnVec::<(&'static str, OwnString<A>), A>::new_in(alloc);
                    for a_idx in 0..attr_count {
                        if let Some(&aname) = ATTR_NAMES.get(a_idx) {
                            let aval = draw_text_value(tape, alloc);
                            owned_attrs.push((aname, aval));
                        }
                    }
                    let mut borrowed_attrs = OwnVec::<(&str, &str), A>::new_in(alloc);
                    for (k, v) in owned_attrs.as_slice() {
                        borrowed_attrs.push((*k, v.as_str()));
                    }
                    writer.start_element(name, borrowed_attrs.as_slice()).map_err(|e| {
                        Broke::Harness(format_in!(alloc, "start_element: {e}").as_str().into())
                    })?;
                    expected.push(ExpectedEvent::Start { name, attrs: owned_attrs, empty: false });
                    open_tags.push(name);
                    last_was_text = false;
                }
                // Empty element
                1 => {
                    let name = TAG_NAMES
                        .get(tape.index(TAG_NAMES.len(), "etag"))
                        .copied()
                        .unwrap_or("leaf-x");
                    let attr_count = tape.int_in(0usize..=2, "eattrs");
                    let mut owned_attrs = OwnVec::<(&'static str, OwnString<A>), A>::new_in(alloc);
                    for a_idx in 0..attr_count {
                        if let Some(&aname) = ATTR_NAMES.get(a_idx) {
                            let aval = draw_text_value(tape, alloc);
                            owned_attrs.push((aname, aval));
                        }
                    }
                    let mut borrowed_attrs = OwnVec::<(&str, &str), A>::new_in(alloc);
                    for (k, v) in owned_attrs.as_slice() {
                        borrowed_attrs.push((*k, v.as_str()));
                    }
                    writer.empty_element(name, borrowed_attrs.as_slice()).map_err(|e| {
                        Broke::Harness(format_in!(alloc, "empty_element: {e}").as_str().into())
                    })?;
                    expected.push(ExpectedEvent::Start { name, attrs: owned_attrs, empty: true });
                    last_was_text = false;
                }
                // Text (avoid adjacent text nodes so 1-to-1 event equality holds)
                2 if !last_was_text => {
                    let val = draw_text_value(tape, alloc);
                    writer.text(val.as_str()).map_err(|e| {
                        Broke::Harness(format_in!(alloc, "text: {e}").as_str().into())
                    })?;
                    expected.push(ExpectedEvent::Text(val));
                    last_was_text = true;
                }
                // CDATA
                3 => {
                    let sample = CDATA_SAMPLES
                        .get(tape.index(CDATA_SAMPLES.len(), "cdata"))
                        .copied()
                        .unwrap_or("raw");
                    writer.cdata(sample).map_err(|e| {
                        Broke::Harness(format_in!(alloc, "cdata: {e}").as_str().into())
                    })?;
                    expected.push(ExpectedEvent::CData(sample));
                    last_was_text = false;
                }
                // Comment
                4 => {
                    let sample = COMMENT_SAMPLES
                        .get(tape.index(COMMENT_SAMPLES.len(), "comment"))
                        .copied()
                        .unwrap_or(" note ");
                    writer.comment(sample).map_err(|e| {
                        Broke::Harness(format_in!(alloc, "comment: {e}").as_str().into())
                    })?;
                    expected.push(ExpectedEvent::Comment(sample));
                    last_was_text = false;
                }
                // Close inner element (keep root open until final unwind)
                _ if open_tags.len() > 1 => {
                    if let Some(name) = open_tags.pop() {
                        writer.end_element().map_err(|e| {
                            Broke::Harness(format_in!(alloc, "end_element: {e}").as_str().into())
                        })?;
                        expected.push(ExpectedEvent::End { name });
                        last_was_text = false;
                    }
                }
                _ => {}
            }
        }

        while let Some(name) = open_tags.pop() {
            writer.end_element().map_err(|e| {
                Broke::Harness(format_in!(alloc, "close remaining: {e}").as_str().into())
            })?;
            expected.push(ExpectedEvent::End { name });
        }

        let doc = writer
            .finish()
            .map_err(|e| Broke::Harness(format_in!(alloc, "finish: {e}").as_str().into()))?;

        let parsed = collect_events(doc.as_str(), XmlLimits::SHIPPED, alloc).map_err(|e| {
            Broke::Oracle(
                format_in!(alloc, "round-trip parse failed ({e}) on: {:?}", doc.as_str())
                    .as_str()
                    .into(),
            )
        })?;

        if parsed.len() != expected.len() {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "event count mismatch: expected {}, got {}",
                    expected.len(),
                    parsed.len()
                )
                .as_str()
                .into(),
            ));
        }

        for (idx, (got, exp)) in parsed.as_slice().iter().zip(expected.as_slice().iter()).enumerate() {
            let matched = match (got, exp) {
                (
                    XmlEvent::StartElement { name, attrs, empty },
                    ExpectedEvent::Start { name: en, attrs: ea, empty: ee },
                ) => {
                    *name == *en
                        && *empty == *ee
                        && attrs.len() == ea.len()
                        && attrs
                            .as_slice()
                            .iter()
                            .zip(ea.as_slice().iter())
                            .all(|(a, (ek, ev))| a.name == *ek && a.value.as_str() == ev.as_str())
                }
                (XmlEvent::EndElement { name }, ExpectedEvent::End { name: en }) => *name == *en,
                (XmlEvent::Text(t), ExpectedEvent::Text(et)) => t.as_str() == et.as_str(),
                (XmlEvent::CData(c), ExpectedEvent::CData(ec)) => *c == *ec,
                (XmlEvent::Comment(c), ExpectedEvent::Comment(ec)) => *c == *ec,
                (
                    XmlEvent::ProcessingInstruction { target, data },
                    ExpectedEvent::Pi { target: et, data: ed },
                ) => *target == *et && *data == *ed,
                _ => false,
            };
            if !matched {
                return Err(Broke::Oracle(
                    format_in!(alloc, "event {idx} mismatch: got {got:?}").as_str().into(),
                ));
            }
        }
        Ok(())
    }

    fn fuzz_hostile_xml<A: Allocator + Copy>(tape: &Tape, alloc: A) {
        const HOSTILE_FRAGS: &[&str] = &[
            "<",
            ">",
            "</",
            "/>",
            "<a>",
            "</a>",
            "</b>",
            "<a x=\"1\"",
            "<a x='&amp;&unknown;&#99999999;&#xZZ;&#0;'>",
            "<!ENTITY x \"y\">",
            "<!DOCTYPE r [<!ENTITY a \"b\">]>",
            "<!DOCTYPE r [<!-- c -->]>",
            "<!-- -- -->",
            "<!--->",
            "<![CDATA[unterminated",
            "<![CDATA[ok]]>",
            "<?xml version=\"1.0\"?>",
            "<?bad",
            "]]>",
            "&amp;",
            "&;",
            "text",
            "\0",
        ];
        let count = tape.int_in(1usize..=12, "hostile_frags");
        let mut input = OwnString::new_in(alloc);
        for _ in 0..count {
            let idx = tape.index(HOSTILE_FRAGS.len(), "hfrag");
            if let Some(&frag) = HOSTILE_FRAGS.get(idx) {
                input.push_str(frag);
            }
        }
        let tight = XmlLimits { most_bytes: 4096, most_depth: 8, most_attrs: 8, most_events: 128 };
        if let Ok(mut reader) = XmlReader::new_in(input.as_str(), tight, alloc) {
            while let Ok(Some(_)) = reader.next_event() {}
        }
    }

    fn check_mismatched_end_tag_oracle<A: Allocator + Copy>(
        tape: &Tape,
        plant_skip_end_tag_check: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let open_idx = tape.index(TAG_NAMES.len(), "open_tag");
        let delta = 1 + tape.index(TAG_NAMES.len() - 1, "delta_tag");
        let close_idx = (open_idx + delta) % TAG_NAMES.len();
        let open_tag = TAG_NAMES.get(open_idx).copied().unwrap_or("root");
        let close_tag = TAG_NAMES.get(close_idx).copied().unwrap_or("item");

        let mut doc = OwnString::new_in(alloc);
        doc.push('<');
        doc.push_str(open_tag);
        doc.push('>');
        if tape.coin("with_child") {
            doc.push_str("<leaf/>");
        }
        doc.push_str("</");
        doc.push_str(close_tag);
        doc.push('>');

        let mut reader = XmlReader::new_in_inner(
            doc.as_str(),
            XmlLimits::SHIPPED,
            alloc,
            plant_skip_end_tag_check,
        )
        .map_err(|e| Broke::Harness(format_in!(alloc, "reader init: {e}").as_str().into()))?;

        loop {
            match reader.next_event() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(Broke::Oracle(
                        format_in!(
                            alloc,
                            "reader accepted mismatched tags <{open_tag}>...</{close_tag}>"
                        )
                        .as_str()
                        .into(),
                    ));
                }
                Err(XmlError::MismatchedEndTag { .. }) => return Ok(()),
                Err(other) => {
                    return Err(Broke::Oracle(
                        format_in!(
                            alloc,
                            "expected MismatchedEndTag for <{open_tag}>...</{close_tag}>, got {other}"
                        )
                        .as_str()
                        .into(),
                    ));
                }
            }
        }
    }

    /// The invariant: Valid XML trees round-trip losslessly through XmlWriter and XmlReader with strict nesting validation, and hostile XML inputs never panic.
    #[test]
    fn property_xml_round_trip_and_hostile_fuzz() {
        let h = heap();
        check(Config::cases(256), |tape| {
            fuzz_hostile_xml(tape, h.alloc());
            build_and_verify_random_tree(tape, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_mismatched_end_tag() {
        let h = heap();
        control(Config::cases(128), false, true, "", |tape, plant| {
            check_mismatched_end_tag_oracle(tape, plant, h.alloc())
        });
    }
}
