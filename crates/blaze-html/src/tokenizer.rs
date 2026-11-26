//! Pure `#![no_std]` bounded HTML5 tokenizer: zero-copy attribute & tag slices, comment &
//! DOCTYPE handling, and raw-text element scanning (`<script>`, `<style>`, `<textarea>`, `<title>`).

use blaze_own::{AVec, Alloc};

/// One raw attribute slice parsed from a start tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawAttr<'src> {
    pub name: &'src str,
    pub value: &'src str,
}

/// One HTML token emitted by [`Tokenizer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token<'src, 'a> {
    StartTag {
        name: &'src str,
        attrs: AVec<'a, RawAttr<'src>>,
        self_closing: bool,
    },
    EndTag {
        name: &'src str,
    },
    Text(&'src str),
    Comment(&'src str),
    Doctype(&'src str),
}

/// Incremental, zero-recursion HTML5 tokenizer over a borrowed `&'src str`.
pub struct Tokenizer<'src> {
    src: &'src str,
    pos: usize,
    raw_text_end: Option<&'static str>,
}

impl<'src> Tokenizer<'src> {
    /// Create a tokenizer over `src`.
    pub const fn new(src: &'src str) -> Self {
        Self { src, pos: 0, raw_text_end: None }
    }

    /// Current byte offset in `src`.
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Advance and return the next token, allocating attribute lists in `alloc`.
    pub fn next_token<'a>(&mut self, max_attrs: usize, alloc: Alloc<'a>) -> Option<Token<'src, 'a>> {
        let bytes = self.src.as_bytes();
        let len = bytes.len();
        if self.pos >= len {
            return None;
        }

        // Inside `<script>`, `<style>`, `<textarea>`, `<title>`: scan verbatim until matching `</tag>`.
        if let Some(close_tag) = self.raw_text_end {
            let start = self.pos;
            if let Some(found) = find_closing_raw_tag(self.src, start, close_tag) {
                self.raw_text_end = None;
                if found > start {
                    self.pos = found;
                    return self.src.get(start..found).map(Token::Text);
                }
                // Fall through immediately to parse the closing `</tag>`.
            } else {
                self.raw_text_end = None;
                self.pos = len;
                return self.src.get(start..len).map(Token::Text);
            }
        }

        if bytes.get(self.pos).copied() != Some(b'<') {
            let start = self.pos;
            while self.pos < len && bytes.get(self.pos).copied() != Some(b'<') {
                self.pos = self.pos.saturating_add(1);
            }
            return self.src.get(start..self.pos).map(Token::Text);
        }

        // Starts with `<`.
        let next_b = bytes.get(self.pos.saturating_add(1)).copied();
        match next_b {
            Some(b'!') => Some(self.scan_bang()),
            Some(b'?') => Some(self.scan_processing_instruction()),
            Some(b'/') => Some(self.scan_end_tag()),
            Some(b) if b.is_ascii_alphabetic() => Some(self.scan_start_tag(max_attrs, alloc)),
            _ => {
                // Lone `<` (e.g. `a < b` in prose): emit `<` plus text until the next `<`.
                let start = self.pos;
                self.pos = self.pos.saturating_add(1);
                while self.pos < len && bytes.get(self.pos).copied() != Some(b'<') {
                    self.pos = self.pos.saturating_add(1);
                }
                self.src.get(start..self.pos).map(Token::Text)
            }
        }
    }

    fn scan_bang<'a>(&mut self) -> Token<'src, 'a> {
        let rest = self.src.get(self.pos..).unwrap_or("");
        // Comment: `<!-- ... -->`
        if let Some(after) = rest.strip_prefix("<!--") {
            let body_start = self.pos.saturating_add(4);
            if let Some(rel) = after.find("-->") {
                let body_end = body_start.saturating_add(rel);
                self.pos = body_end.saturating_add(3);
                return Token::Comment(self.src.get(body_start..body_end).unwrap_or(""));
            }
            self.pos = self.src.len();
            return Token::Comment(self.src.get(body_start..).unwrap_or(""));
        }
        // CDATA: `<![CDATA[ ... ]]>` -> treat body as Text
        if let Some(after) = rest.strip_prefix("<![CDATA[") {
            let body_start = self.pos.saturating_add(9);
            if let Some(rel) = after.find("]]>") {
                let body_end = body_start.saturating_add(rel);
                self.pos = body_end.saturating_add(3);
                return Token::Text(self.src.get(body_start..body_end).unwrap_or(""));
            }
            self.pos = self.src.len();
            return Token::Text(self.src.get(body_start..).unwrap_or(""));
        }
        // DOCTYPE or bogus comment: scan to `>`.
        let start = self.pos.saturating_add(2);
        let end = rest.find('>').map_or(self.src.len(), |r| self.pos.saturating_add(r));
        self.pos = if end < self.src.len() { end.saturating_add(1) } else { end };
        let inner = self.src.get(start..end).unwrap_or("").trim();
        if inner.len() >= 7
            && inner.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("doctype"))
        {
            Token::Doctype(inner)
        } else {
            Token::Comment(inner)
        }
    }

    fn scan_processing_instruction<'a>(&mut self) -> Token<'src, 'a> {
        let rest = self.src.get(self.pos..).unwrap_or("");
        let start = self.pos.saturating_add(2);
        let end = rest.find('>').map_or(self.src.len(), |r| self.pos.saturating_add(r));
        self.pos = if end < self.src.len() { end.saturating_add(1) } else { end };
        Token::Comment(self.src.get(start..end).unwrap_or(""))
    }

    fn scan_end_tag<'a>(&mut self) -> Token<'src, 'a> {
        let bytes = self.src.as_bytes();
        let len = bytes.len();
        self.pos = self.pos.saturating_add(2); // skip `</`
        self.skip_ws();
        let name_start = self.pos;
        while self.pos < len && bytes.get(self.pos).copied().is_some_and(is_tag_name_byte) {
            self.pos = self.pos.saturating_add(1);
        }
        let name = self.src.get(name_start..self.pos).unwrap_or("");
        while self.pos < len && bytes.get(self.pos).copied() != Some(b'>') {
            self.pos = self.pos.saturating_add(1);
        }
        if self.pos < len {
            self.pos = self.pos.saturating_add(1);
        }
        Token::EndTag { name }
    }

    fn scan_start_tag<'a>(&mut self, max_attrs: usize, alloc: Alloc<'a>) -> Token<'src, 'a> {
        let bytes = self.src.as_bytes();
        let len = bytes.len();
        self.pos = self.pos.saturating_add(1); // skip `<`
        let name_start = self.pos;
        while self.pos < len && bytes.get(self.pos).copied().is_some_and(is_tag_name_byte) {
            self.pos = self.pos.saturating_add(1);
        }
        let name = self.src.get(name_start..self.pos).unwrap_or("");
        let mut attrs = AVec::new_in(alloc);
        let mut self_closing = false;

        loop {
            self.skip_ws();
            let Some(b) = bytes.get(self.pos).copied() else {
                break;
            };
            if b == b'>' {
                self.pos = self.pos.saturating_add(1);
                break;
            }
            if b == b'/' {
                self.pos = self.pos.saturating_add(1);
                if bytes.get(self.pos).copied() == Some(b'>') {
                    self_closing = true;
                    self.pos = self.pos.saturating_add(1);
                    break;
                }
                continue;
            }

            // Attribute name.
            let a_start = self.pos;
            while self.pos < len
                && bytes.get(self.pos).copied().is_some_and(is_attr_name_byte)
            {
                self.pos = self.pos.saturating_add(1);
            }
            if self.pos == a_start {
                // Unexpected stray byte inside tag: skip to guarantee progress (rule 18).
                self.pos = self.pos.saturating_add(1);
                continue;
            }
            let attr_name = self.src.get(a_start..self.pos).unwrap_or("");
            self.skip_ws();

            let attr_val = if bytes.get(self.pos).copied() == Some(b'=') {
                self.pos = self.pos.saturating_add(1);
                self.skip_ws();
                match bytes.get(self.pos).copied() {
                    Some(q @ (b'"' | b'\'')) => {
                        self.pos = self.pos.saturating_add(1);
                        let v_start = self.pos;
                        while self.pos < len && bytes.get(self.pos).copied() != Some(q) {
                            self.pos = self.pos.saturating_add(1);
                        }
                        let val = self.src.get(v_start..self.pos).unwrap_or("");
                        if self.pos < len {
                            self.pos = self.pos.saturating_add(1);
                        }
                        val
                    }
                    _ => {
                        let v_start = self.pos;
                        while self.pos < len
                            && bytes.get(self.pos).copied().is_some_and(|c| {
                                !c.is_ascii_whitespace() && c != b'>'
                            })
                        {
                            self.pos = self.pos.saturating_add(1);
                        }
                        self.src.get(v_start..self.pos).unwrap_or("")
                    }
                }
            } else {
                ""
            };

            if attrs.len() < max_attrs {
                attrs.push(RawAttr { name: attr_name, value: attr_val });
            }
        }

        if !self_closing {
            if name.eq_ignore_ascii_case("script") {
                self.raw_text_end = Some("script");
            } else if name.eq_ignore_ascii_case("style") {
                self.raw_text_end = Some("style");
            } else if name.eq_ignore_ascii_case("textarea") {
                self.raw_text_end = Some("textarea");
            } else if name.eq_ignore_ascii_case("title") {
                self.raw_text_end = Some("title");
            }
        }

        Token::StartTag { name, attrs, self_closing }
    }

    fn skip_ws(&mut self) {
        let bytes = self.src.as_bytes();
        while bytes.get(self.pos).copied().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos = self.pos.saturating_add(1);
        }
    }
}

fn is_tag_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':'
}

fn is_attr_name_byte(b: u8) -> bool {
    !b.is_ascii_whitespace() && !matches!(b, b'=' | b'>' | b'/' | b'"' | b'\'')
}

fn find_closing_raw_tag(src: &str, from: usize, tag: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let tlen = tag.len();
    let mut i = from;
    while i < len {
        if bytes.get(i).copied() == Some(b'<')
            && bytes.get(i.saturating_add(1)).copied() == Some(b'/')
        {
            let name_start = i.saturating_add(2);
            let name_end = name_start.saturating_add(tlen);
            if let Some(slice) = src.get(name_start..name_end)
                && slice.eq_ignore_ascii_case(tag)
                && bytes
                    .get(name_end)
                    .copied()
                    .is_some_and(|b| b == b'>' || b.is_ascii_whitespace() || b == b'/')
            {
                return Some(i);
            }
        }
        i = i.saturating_add(1);
    }
    None
}
