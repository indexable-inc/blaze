//! Reads LIR's text form ([`crate::print`]) back to its columns. A function printed and
//! parsed is the same bytes; text written by hand is any function the grammar spells, which
//! the verifier then judges (`parse` checks the text, not what it means).

use std::{alloc::Global, fmt};

use blaze_format::SpanRef;
use blaze_ids::ContentId;

use crate::{
    flags::{self, AtomicRmw, FloatCond, IntCond},
    func::{
        AsmOperandKind, AsmOperandRec, AsmPieceRec, AsmRec, BlockKind, CompoundRec, Conv, DataRec,
        DataRelocRec, Ext, FnAttrs, Func, Head, Inline, InlinedRec, IntrinsicRec, MemAttrsRec, NONE,
        ParamFactsRec, ParamKind, ParamRec, RangeRec, RefKind, RefRec, SigRec, SlotKind, SlotRec,
        VarRec,
    },
    op::{Field, Layout, Op},
    ty::{Scalar, Ty},
};

/// Why text is not LIR's text form.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError {
    /// The line, from 1.
    pub line: usize,
    /// What was wanted there.
    pub what: &'static str,
    /// What was found instead.
    pub found: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {} wanted, `{}` found", self.line, self.what, self.found)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

#[derive(Clone, PartialEq, Eq, Debug)]
enum Tok<'a> {
    Word(&'a str),
    Str(String),
    Punct(char),
}

impl fmt::Display for Tok<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Word(word) => f.write_str(word),
            Tok::Str(text) => write!(f, "{text:?}"),
            Tok::Punct(c) => write!(f, "{c}"),
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '+')
}

fn lex(line: &str, number: usize) -> Result<Vec<Tok<'_>>> {
    // alloc: one line's tokens, parse being a test and tool path
    let mut toks = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(at, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if is_word(c) {
            let mut end = at;
            while let Some(&(i, c)) = chars.peek() {
                if !is_word(c) {
                    break;
                }
                end = i + c.len_utf8();
                chars.next();
            }
            toks.push(Tok::Word(&line[at..end]));
        } else if c == '"' {
            chars.next();
            // alloc: as above
            let mut text = String::new();
            let mut closed = false;
            while let Some((_, c)) = chars.next() {
                match c {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\\' => match chars.next() {
                        Some((_, '"')) => text.push('"'),
                        Some((_, '\\')) => text.push('\\'),
                        Some((_, 'x')) => {
                            let hi = chars.next().and_then(|it| it.1.to_digit(16));
                            let lo = chars.next().and_then(|it| it.1.to_digit(16));
                            let (Some(hi), Some(lo)) = (hi, lo) else {
                                return Err(error(number, "two hex digits after \\x", line));
                            };
                            text.push(char::from((hi * 16 + lo) as u8));
                        }
                        _ => return Err(error(number, "an escape (\\\\, \\\" or \\xNN)", line)),
                    },
                    c => text.push(c),
                }
            }
            if !closed {
                return Err(error(number, "a closing quote", line));
            }
            toks.push(Tok::Str(text));
        } else if "()[]{},:=<>!@".contains(c) {
            chars.next();
            toks.push(Tok::Punct(c));
        } else {
            return Err(error(number, "a word, a string or punctuation", &line[at..]));
        }
    }
    Ok(toks)
}

fn error(line: usize, what: &'static str, found: &str) -> ParseError {
    // alloc: the error's text, once
    ParseError { line, what, found: found.to_owned() }
}

/// One line's tokens, read in order.
struct Line<'t, 'a> {
    toks: &'t [Tok<'a>],
    at: usize,
    line: usize,
}

impl<'t, 'a> Line<'t, 'a> {
    fn err(&self, what: &'static str) -> ParseError {
        let found = self.toks.get(self.at).map_or_else(|| "the end of the line".to_owned(), |it| it.to_string()); // alloc: the error's text, once
        ParseError { line: self.line, what, found }
    }

    fn peek(&self) -> Option<&'t Tok<'a>> {
        self.toks.get(self.at)
    }

    fn word(&mut self, what: &'static str) -> Result<&'a str> {
        if let Some(Tok::Word(word)) = self.peek() {
            self.at += 1;
            return Ok(*word);
        }
        Err(self.err(what))
    }

    fn keyword(&mut self, keyword: &'static str) -> Result<()> {
        if self.is_keyword(keyword) {
            self.at += 1;
            return Ok(());
        }
        Err(self.err(keyword))
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(word)) if *word == keyword)
    }

    fn punct(&mut self, c: char, what: &'static str) -> Result<()> {
        if self.is_punct(c) {
            self.at += 1;
            return Ok(());
        }
        Err(self.err(what))
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(it)) if *it == c)
    }

    fn string(&mut self) -> Result<&'t str> {
        if let Some(Tok::Str(text)) = self.peek() {
            self.at += 1;
            return Ok(text.as_str());
        }
        Err(self.err("a quoted string"))
    }

    fn number<T: std::str::FromStr>(&mut self, what: &'static str) -> Result<T> {
        let at = self.at;
        let word = self.word(what)?;
        word.parse().map_err(|_| {
            self.at = at;
            self.err(what)
        })
    }

    /// `prefix` then a decimal: `v3`, `block2`, `ref0`.
    fn id(&mut self, prefix: &str, what: &'static str) -> Result<u32> {
        let at = self.at;
        let word = self.word(what)?;
        match word.strip_prefix(prefix).filter(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit())) {
            Some(digits) => digits.parse().map_err(|_| {
                self.at = at;
                self.err(what)
            }),
            None => {
                self.at = at;
                Err(self.err(what))
            }
        }
    }

    /// `none` as [`NONE`], or a decimal.
    fn none_or(&mut self, what: &'static str) -> Result<u32> {
        if self.is_keyword("none") {
            self.at += 1;
            return Ok(NONE);
        }
        self.number(what)
    }

    /// `none` as 0, or `{prefix}N` as 1 + N.
    fn row_or_none(&mut self, prefix: &str, what: &'static str) -> Result<u32> {
        if self.is_keyword("none") {
            self.at += 1;
            return Ok(0);
        }
        Ok(self.id(prefix, what)? + 1)
    }

    fn ty(&mut self) -> Result<Ty> {
        if self.is_punct('<') {
            self.at += 1;
            let scalable = self.is_keyword("vscale");
            if scalable {
                self.at += 1;
                self.keyword("x")?;
            }
            let lanes: u32 = self.number("a lane count")?;
            self.keyword("x")?;
            let elem = self.word("a lane type")?;
            let elem = Scalar::from_name(elem).ok_or_else(|| self.err("a lane type"))?;
            self.punct('>', "`>`")?;
            let ty = match scalable {
                true => Ty::scalable(elem, lanes),
                false => Ty::vector(elem, lanes),
            };
            return ty.ok_or_else(|| self.err("a vector LIR has"));
        }
        let at = self.at;
        let word = self.word("a type")?;
        if let Some(row) = word.strip_prefix('t').and_then(|it| it.parse::<u32>().ok()) {
            return Ty::compound(row).ok_or_else(|| self.err("a compound row below 2^15"));
        }
        match Scalar::from_name(word) {
            Some(Scalar::None) | None => {
                self.at = at;
                Err(self.err("a type"))
            }
            Some(scalar) => Ok(Ty::scalar(scalar)),
        }
    }

    fn hex_id(&mut self) -> Result<ContentId> {
        let at = self.at;
        let word = self.word("a content ID in hex")?;
        let mut id = [0u8; 32];
        let bad = word.len() != 64
            || (0..32).any(|i| match u8::from_str_radix(&word[2 * i..2 * i + 2], 16) {
                Ok(byte) => {
                    id[i] = byte;
                    false
                }
                Err(_) => true,
            });
        match bad || !word.is_ascii() {
            true => {
                self.at = at;
                Err(self.err("a content ID in hex"))
            }
            false => Ok(ContentId(id)),
        }
    }

    /// `0x` and hex words, the most significant first: the words little-endian.
    fn hex_words(&mut self, words: u32) -> Result<Vec<u32>> {
        let at = self.at;
        let word = self.word("a constant in hex")?;
        let digits = word.strip_prefix("0x").filter(|it| it.len() == 8 * words as usize && it.is_ascii());
        let Some(digits) = digits else {
            self.at = at;
            return Err(self.err("a constant of its type's words in hex"));
        };
        // alloc: a constant's words, parse being a test and tool path
        let mut out = Vec::with_capacity(words as usize);
        for i in (0..words as usize).rev() {
            let word = u32::from_str_radix(&digits[8 * i..8 * i + 8], 16).map_err(|_| {
                self.at = at;
                self.err("a constant in hex")
            })?;
            out.push(word);
        }
        Ok(out)
    }

    fn end(&self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(_) => Err(self.err("the end of the line")),
        }
    }

    /// `[` items separated by commas `]`.
    fn list(&mut self, open: char, close: char, mut item: impl FnMut(&mut Self) -> Result<()>) -> Result<()> {
        self.punct(open, "the start of a list")?;
        let mut first = true;
        while !self.is_punct(close) {
            if !first {
                self.punct(',', "`,`")?;
            }
            item(self)?;
            first = false;
        }
        self.punct(close, "the end of a list")
    }

    /// Bit names until the end of the line.
    fn bits(&mut self, named: &[(u8, &str)]) -> Result<u8> {
        let mut bits = 0;
        while self.peek().is_some() {
            let name = self.word("a flag name")?;
            let &(bit, _) = named.iter().find(|it| it.1 == name).ok_or_else(|| {
                self.at -= 1;
                self.err("a flag name")
            })?;
            bits |= bit;
        }
        Ok(bits)
    }
}

/// Reads a function from its text form.
pub fn parse(text: &str) -> Result<Func<Global>> {
    // alloc: the function, parse being a test and tool path
    let mut func = Func::new_in(Head::default(), Global);
    let mut header = false;
    let mut in_block = false;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let toks = lex(raw, number)?;
        let mut line = Line { toks: &toks, at: 0, line: number };
        if !header {
            parse_header(&mut line, &mut func)?;
            header = true;
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            if !in_block {
                return Err(line.err("a block before its instructions"));
            }
            instruction(&mut line, &mut func)?;
            continue;
        }
        let first = line.word("a table row or a block")?;
        if first.starts_with("block") && !first["block".len()..].is_empty() && first["block".len()..].bytes().all(|b| b.is_ascii_digit()) {
            line.at = 0;
            let id = line.id("block", "a block")?;
            if id as usize != func.block_starts.len() {
                return Err(line.err("blocks in order"));
            }
            let kind = line.word("a block kind")?;
            let kind = BlockKind::ALL.into_iter().find(|it| it.name() == kind).ok_or_else(|| line.err("a block kind"))?;
            line.punct(':', "`:`")?;
            line.end()?;
            func.block_starts.push(func.ops.len() as u32);
            func.block_kinds.push(kind as u8);
            in_block = true;
            continue;
        }
        if in_block {
            return Err(line.err("an indented instruction: tables come before blocks"));
        }
        table(first, &mut line, &mut func)?;
    }
    if !header {
        return Err(error(1, "a `func` header", ""));
    }
    Ok(func)
}

fn parse_header(line: &mut Line<'_, '_>, func: &mut Func<Global>) -> Result<()> {
    line.keyword("func")?;
    func.head.ptr_bits = line.id("ptr", "the pointer width, `ptr64`")?;
    line.keyword("sig")?;
    func.head.sig = line.id("s", "the function's signature")?;
    line.keyword("attrs")?;
    line.punct('[', "`[`")?;
    let mut attrs = FnAttrs(0);
    while !line.is_punct(']') {
        let name = line.word("a function attribute")?;
        if let Some(&(bit, _)) = FnAttrs::NAMED.iter().find(|it| it.1 == name) {
            attrs = FnAttrs(attrs.0 | bit);
        } else if let Some(inline) =
            [Inline::Hint, Inline::Always, Inline::Never].into_iter().find(|it| it.name() == name)
        {
            attrs = attrs.with_inline(inline);
        } else {
            line.at -= 1;
            return Err(line.err("a function attribute"));
        }
    }
    line.punct(']', "`]`")?;
    func.head.attrs = attrs.0;
    line.keyword("features")?;
    let features = line.string()?;
    func.features.extend_from_slice(features.as_bytes());
    line.end()
}

/// The row number of a table's line, which must be the table's next.
fn next_row(line: &mut Line<'_, '_>, prefix: &str, len: usize) -> Result<()> {
    let row = line.id(prefix, "the table's row")?;
    if row as usize != len {
        line.at -= 1;
        return Err(line.err("the table's next row"));
    }
    line.punct('=', "`=`")
}

fn param(line: &mut Line<'_, '_>) -> Result<ParamRec> {
    let ty = line.ty()?;
    let mut rec = ParamRec { facts: 0, ty: ty.0, ext: Ext::None as u8, kind: ParamKind::Value as u8 };
    loop {
        match line.peek() {
            Some(Tok::Word("zext")) => rec.ext = Ext::Zext as u8,
            Some(Tok::Word("sext")) => rec.ext = Ext::Sext as u8,
            Some(Tok::Word("sret")) => rec.kind = ParamKind::Sret as u8,
            Some(Tok::Word("byval")) => rec.kind = ParamKind::Byval as u8,
            Some(Tok::Word(word)) if word.starts_with('f') => {
                rec.facts = line.id("f", "a parameter's facts")? + 1;
                continue;
            }
            Some(Tok::Word(_) | Tok::Str(_) | Tok::Punct(_)) | None => return Ok(rec),
        }
        line.at += 1;
    }
}

fn span_ref(line: &mut Line<'_, '_>) -> Result<SpanRef> {
    let anchor = line.hex_id()?;
    let index = line.number("a span's index")?;
    Ok(SpanRef { anchor, index })
}

fn table(keyword: &str, line: &mut Line<'_, '_>, func: &mut Func<Global>) -> Result<()> {
    match keyword {
        "type" => {
            next_row(line, "t", func.compounds.len())?;
            let first = func.compound_tys.len() as u32;
            line.list('{', '}', |line| {
                let ty = line.ty()?;
                func.compound_tys.push(ty.0);
                Ok(())
            })?;
            let len = func.compound_tys.len() as u32 - first;
            func.compounds.push(CompoundRec { first, len });
        }
        "range" => {
            next_row(line, "r", func.ranges.len())?;
            let start = line.number("a range's start")?;
            line.punct(',', "`,`")?;
            let end = line.number("a range's end")?;
            func.ranges.push(RangeRec { start, end });
        }
        "facts" => {
            next_row(line, "f", func.param_facts.len())?;
            line.keyword("deref")?;
            let deref = line.number("a dereferenceable size")?;
            line.keyword("size")?;
            let size = line.number("a pointee size")?;
            line.keyword("align")?;
            let align = line.number("an alignment's log2")?;
            line.keyword("range")?;
            let range = line.row_or_none("r", "a range")?;
            let bits = line.bits(&ParamFactsRec::NAMED)?;
            func.param_facts.push(ParamFactsRec { deref, size, range, align, bits, pad: [0; 6] });
        }
        "sig" => {
            next_row(line, "s", func.sigs.len())?;
            let conv = line.word("a calling convention")?;
            let conv = Conv::from_name(conv).ok_or_else(|| {
                line.at -= 1;
                line.err("a calling convention")
            })?;
            let first = func.sig_params.len() as u32;
            line.list('(', ')', |line| {
                let rec = param(line)?;
                func.sig_params.push(rec);
                Ok(())
            })?;
            let params = func.sig_params.len() as u32 - first;
            line.keyword("-")?;
            line.punct('>', "`->`")?;
            line.list('(', ')', |line| {
                let rec = param(line)?;
                func.sig_params.push(rec);
                Ok(())
            })?;
            let rets = func.sig_params.len() as u32 - first - params;
            let bits = match line.is_keyword("variadic") {
                true => {
                    line.at += 1;
                    SigRec::VARIADIC
                }
                false => 0,
            };
            let (Ok(params), Ok(rets)) = (u16::try_from(params), u16::try_from(rets)) else {
                return Err(line.err("a signature of fewer than 2^16 parameters"));
            };
            func.sigs.push(SigRec { first, params, rets, conv: conv as u8, bits, pad: [0; 2] });
        }
        "mem" => {
            next_row(line, "m", func.mem_attrs.len())?;
            line.keyword("align")?;
            let align = line.number("an alignment's log2")?;
            line.keyword("src_align")?;
            let src_align = line.number("an alignment's log2")?;
            line.keyword("pointee_align")?;
            let pointee_align = line.number("an alignment's log2")?;
            line.keyword("range")?;
            let range = line.row_or_none("r", "a range")?;
            let bits = line.bits(&MemAttrsRec::NAMED)?;
            func.mem_attrs.push(MemAttrsRec { range, align, src_align, pointee_align, bits });
        }
        "ref" => {
            next_row(line, "ref", func.refs.len())?;
            let kind = line.word("a reference's kind")?;
            let kind = RefKind::from_name(kind).ok_or_else(|| {
                line.at -= 1;
                line.err("a reference's kind")
            })?;
            let place = match kind {
                RefKind::Data => line.id("d", "a data row")?,
                RefKind::Func | RefKind::Static | RefKind::Tls => line.number("a place")?,
            };
            func.refs.push(RefRec { place, kind: kind as u32 });
        }
        "data" => {
            next_row(line, "d", func.datas.len())?;
            line.keyword("align")?;
            let align = line.number("an alignment's log2")?;
            line.keyword("bytes")?;
            let hex = line.string()?;
            let bytes_first = func.data_bytes.len() as u32;
            if hex.len() % 2 != 0 || !hex.is_ascii() {
                line.at -= 1;
                return Err(line.err("bytes in hex"));
            }
            for i in 0..hex.len() / 2 {
                let byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| {
                    line.at -= 1;
                    line.err("bytes in hex")
                })?;
                func.data_bytes.push(byte);
            }
            line.keyword("relocs")?;
            let relocs_first = func.data_relocs.len() as u32;
            line.list('[', ']', |line| {
                let offset = line.number("an address's offset")?;
                let target = line.id("ref", "an address's reference")?;
                let addend = line.number("an address's addend")?;
                func.data_relocs.push(DataRelocRec { addend, offset, target });
                Ok(())
            })?;
            func.datas.push(DataRec {
                bytes_first,
                bytes_len: func.data_bytes.len() as u32 - bytes_first,
                relocs_first,
                relocs_len: func.data_relocs.len() as u32 - relocs_first,
                align,
            });
        }
        "intrinsic" => {
            next_row(line, "i", func.intrinsics.len())?;
            let name = line.string()?;
            let sig = line.id("s", "an intrinsic's signature")?;
            let name_first = func.names.len() as u32;
            func.names.extend_from_slice(name.as_bytes());
            func.intrinsics.push(IntrinsicRec { name_first, name_len: name.len() as u32, sig });
        }
        "slot" => {
            next_row(line, "ss", func.slots.len())?;
            let size = line.number("a slot's size")?;
            line.keyword("align")?;
            let align = line.number("an alignment's log2")?;
            let kind = line.word("a slot's kind")?;
            let kind = SlotKind::from_name(kind).ok_or_else(|| {
                line.at -= 1;
                line.err("a slot's kind")
            })?;
            func.slots.push(SlotRec { size, align, kind: kind as u8, pad: [0; 2] });
        }
        "asm" => asm(line, func)?,
        "span" => {
            next_row(line, "sp", func.spans.len())?;
            let span = span_ref(line)?;
            func.spans.push(span);
            if line.is_keyword("scope") {
                line.at += 1;
                let scope = line.number("a span's scope")?;
                func.span_scopes.push(scope);
            }
            if line.is_keyword("inlined") {
                line.at += 1;
                let inlined = line.none_or("a span's inlined body")?;
                func.span_inlined.push(inlined);
            }
        }
        "var" => {
            next_row(line, "x", func.variables.len())?;
            line.keyword("binding")?;
            let binding = line.number("a binding")?;
            line.keyword("slot")?;
            let slot = line.id("ss", "a slot")?;
            line.keyword("pointer")?;
            let pointer = line.none_or("an offset from a pointer")?;
            line.keyword("scope")?;
            let scope = line.none_or("a scope")?;
            line.keyword("param")?;
            let param = line.number("0 or 1")?;
            line.keyword("ty")?;
            let ty = line.hex_id()?;
            func.variables.push(VarRec { binding, slot, pointer, scope, param, ty });
        }
        "scope" => {
            next_row(line, "sc", func.scope_parents.len())?;
            line.keyword("parent")?;
            let parent = line.none_or("a parent scope")?;
            func.scope_parents.push(parent);
        }
        "inline" => {
            next_row(line, "in", func.inlined.len())?;
            line.keyword("parent")?;
            let parent = line.none_or("a parent inlined body")?;
            line.keyword("site")?;
            let call_site = span_ref(line)?;
            func.inlined.push(InlinedRec { parent, call_site });
        }
        _ => {
            line.at = 0;
            return Err(line.err("a table's keyword, a block or an indented instruction"));
        }
    }
    line.end()
}

/// An `asm` row: its text goes in as the builder writes it, its template's text pieces,
/// then its operands' registers, then its clobbers.
fn asm(line: &mut Line<'_, '_>, func: &mut Func<Global>) -> Result<()> {
    next_row(line, "a", func.asms.len())?;
    line.keyword("options")?;
    line.punct('[', "`[`")?;
    let mut options = 0;
    while !line.is_punct(']') {
        let name = line.word("an asm option")?;
        let &(bit, _) = AsmRec::NAMED.iter().find(|it| it.1 == name).ok_or_else(|| {
            line.at -= 1;
            line.err("an asm option")
        })?;
        options |= bit;
    }
    line.punct(']', "`]`")?;
    line.keyword("clobbers")?;
    let clobbers = line.string()?;
    line.keyword("pieces")?;
    let pieces_first = func.asm_pieces.len() as u32;
    line.list('[', ']', |line| {
        let piece = match line.peek() {
            Some(Tok::Str(text)) => {
                line.at += 1;
                let a = func.asm_text.len() as u32;
                func.asm_text.extend_from_slice(text.as_bytes());
                AsmPieceRec { kind: AsmPieceRec::TEXT, a, b: text.len() as u32 }
            }
            Some(Tok::Word(_) | Tok::Punct(_)) | None => {
                line.punct('{', "a template's text or `{operand}`")?;
                let a = line.number("an operand's place")?;
                let mut b = 0;
                if line.is_punct(':') {
                    line.at += 1;
                    let modifier = line.word("a modifier")?;
                    let mut chars = modifier.chars();
                    let (Some(c), None) = (chars.next(), chars.next()) else {
                        line.at -= 1;
                        return Err(line.err("a modifier of one character"));
                    };
                    b = c as u32;
                }
                line.punct('}', "`}`")?;
                AsmPieceRec { kind: AsmPieceRec::OPERAND, a, b }
            }
        };
        func.asm_pieces.push(piece);
        Ok(())
    })?;
    line.keyword("operands")?;
    let operands_first = func.asm_operands.len() as u32;
    line.list('[', ']', |line| {
        let kind = line.word("an operand's kind")?;
        let kind = AsmOperandKind::from_name(kind).ok_or_else(|| {
            line.at -= 1;
            line.err("an operand's kind")
        })?;
        let class = match line.word("`reg` or `class`")? {
            "reg" => 0,
            "class" => 1,
            _ => {
                line.at -= 1;
                return Err(line.err("`reg` or `class`"));
            }
        };
        let reg = line.string()?;
        let aux = line.number("an operand's aux word")?;
        let reg_first = func.asm_text.len() as u32;
        func.asm_text.extend_from_slice(reg.as_bytes());
        func.asm_operands.push(AsmOperandRec {
            reg_first,
            reg_len: reg.len() as u32,
            aux,
            kind: kind as u8,
            class,
            pad: [0; 2],
        });
        Ok(())
    })?;
    let clobbers_first = func.asm_text.len() as u32;
    func.asm_text.extend_from_slice(clobbers.as_bytes());
    func.asms.push(AsmRec {
        pieces_first,
        pieces_len: func.asm_pieces.len() as u32 - pieces_first,
        operands_first,
        operands_len: func.asm_operands.len() as u32 - operands_first,
        options,
        clobbers_first,
        clobbers_len: clobbers.len() as u32,
    });
    Ok(())
}

/// Reads a target: `blockN(args)`.
fn target(line: &mut Line<'_, '_>, words: &mut Vec<u32>) -> Result<()> {
    words.push(line.id("block", "a block")?);
    let count = words.len();
    words.push(0);
    line.list('(', ')', |line| {
        words.push(line.id("v", "a value")?);
        Ok(())
    })?;
    words[count] = (words.len() - count - 1) as u32;
    Ok(())
}

fn instruction(line: &mut Line<'_, '_>, func: &mut Func<Global>) -> Result<()> {
    let at = func.ops.len();
    let has_value = matches!(line.toks.get(1), Some(Tok::Punct('=')));
    if has_value {
        let value = line.id("v", "the instruction's value")?;
        if value as usize != at {
            line.at -= 1;
            return Err(line.err("the instruction's own index as its value"));
        }
        line.punct('=', "`=`")?;
    }
    let spelled = line.word("an operation")?;
    let mut parts = spelled.split('.');
    let name = parts.next().unwrap_or("");
    let Some(op) = Op::from_name(name) else {
        line.at -= 1;
        return Err(line.err("an operation"));
    };
    // alloc: a flag name list, parse being a test and tool path
    let names: Vec<&str> = parts.collect();
    let info = op.info();
    let flags = flags::parse(info.flags, &names).map_err(|_| {
        line.at -= 1;
        line.err("flags its operation's class has")
    })?;
    let ty = match has_value {
        true => line.ty()?,
        false => Ty::NONE,
    };
    // alloc: one instruction's operand words, parse being a test and tool path
    let mut words: Vec<u32> = Vec::new();
    let mut scrutinee = None;
    for (i, &field) in info.layout.fields().iter().enumerate() {
        if i > 0 {
            line.punct(',', "`,` between operands")?;
        }
        match field {
            Field::Value => {
                let value = line.id("v", "a value")?;
                scrutinee = Some(value);
                words.push(value);
            }
            Field::Imm => words.push(line.number("an immediate")?),
            Field::Offset => words.push(line.number::<i32>("a signed offset")? as u32),
            Field::IntCond => {
                let name = line.word("a comparison")?;
                let cond = IntCond::from_name(name).ok_or_else(|| line.err("a comparison"))?;
                words.push(cond as u32);
            }
            Field::FloatCond => {
                let name = line.word("a float comparison")?;
                let cond = FloatCond::from_name(name).ok_or_else(|| line.err("a float comparison"))?;
                words.push(cond as u32);
            }
            Field::Rmw => {
                let name = line.word("an atomic operation")?;
                let rmw = AtomicRmw::from_name(name).ok_or_else(|| line.err("an atomic operation"))?;
                words.push(rmw as u32);
            }
            Field::Ref => words.push(line.id("ref", "a reference")?),
            Field::Sig => words.push(line.id("s", "a signature")?),
            Field::Slot => words.push(line.id("ss", "a slot")?),
            Field::Intrinsic => words.push(line.id("i", "an intrinsic")?),
            Field::Asm => words.push(line.id("a", "an asm block")?),
            Field::Block => words.push(line.id("block", "a block")?),
            Field::Target => target(line, &mut words)?,
            Field::Values | Field::Blocks | Field::Mask => {
                let count = words.len();
                words.push(0);
                let (open, close) = if field == Field::Values { ('(', ')') } else { ('[', ']') };
                line.list(open, close, |line| {
                    let word = if field == Field::Values {
                        line.id("v", "a value")?
                    } else if field == Field::Blocks {
                        line.id("block", "a block")?
                    } else {
                        line.number("a lane")?
                    };
                    words.push(word);
                    Ok(())
                })?;
                words[count] = (words.len() - count - 1) as u32;
            }
            Field::Const => {
                let constant = line.hex_words(ty.const_words())?;
                words.extend_from_slice(&constant);
            }
            Field::Cases => {
                let width = scrutinee
                    .and_then(|it| func.tys.get(it as usize))
                    .map_or(0, |&it| Ty(it).const_words());
                let count = words.len();
                words.push(0);
                let mut cases = 0u32;
                line.list('[', ']', |line| {
                    let value = line.hex_words(width)?;
                    words.extend_from_slice(&value);
                    line.punct(':', "`:`")?;
                    target(line, &mut words)?;
                    cases += 1;
                    Ok(())
                })?;
                words[count] = cases;
            }
        }
    }
    let mut attrs = 0;
    let mut loc = 0;
    if line.is_punct('!') {
        line.at += 1;
        attrs = line.id("m", "memory attributes")? + 1;
    }
    if line.is_punct('@') {
        line.at += 1;
        loc = line.id("sp", "a span")? + 1;
    }
    line.end()?;
    let data = match info.layout {
        Layout::None => 0,
        Layout::Data(_) => words.first().copied().unwrap_or(0),
        Layout::Extra(_) => {
            let start = func.extra.len() as u32;
            func.extra.extend_from_slice(&words);
            start
        }
    };
    func.ops.push(op as u8);
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    Ok(())
}
