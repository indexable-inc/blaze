//! The encoding `TopSubtree` had before its columns, kept as the model they are tested against:
//! every token tree a variable-length record, written from the end to the beginning so that a
//! subtree knows how many bytes its children take; the parts of spans without their ranges and
//! the symbols in tables sorted by use.
//!
//! The bytes are the old ones; the writer and reader are safe versions of the old ones.

use blaze_ids::radix;
use intern::Symbol;
use blaze_hash::HashMap;
use span::{Span, TextRange, TextSize};

use super::CompressedSpanPart;
use crate::{
    Delimiter, DelimiterKind, Ident, IdentIsRaw, Leaf, LitKind, Literal, Punct, Spacing, Subtree,
    TokenTree,
};

/// Bytes written back to front: each value's bytes are kept reversed, and the whole reversed at
/// the end.
struct Writer {
    reversed: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, value: u8) {
        self.reversed.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.reversed.extend(value.to_ne_bytes().iter().rev());
    }

    fn u32(&mut self, value: u32) {
        self.reversed.extend(value.to_ne_bytes().iter().rev());
    }

    fn len(&self) -> usize {
        self.reversed.len()
    }

    fn finish(mut self) -> Box<[u8]> {
        self.reversed.reverse();
        self.reversed.into_boxed_slice()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn u8(&mut self) -> u8 {
        let (first, rest) = self.bytes.split_first().expect("a byte to read");
        self.bytes = rest;
        *first
    }

    fn u16(&mut self) -> u16 {
        let (value, rest) = self.bytes.split_first_chunk::<2>().expect("two bytes to read");
        self.bytes = rest;
        u16::from_ne_bytes(*value)
    }

    fn u32(&mut self) -> u32 {
        let (value, rest) = self.bytes.split_first_chunk::<4>().expect("four bytes to read");
        self.bytes = rest;
        u32::from_ne_bytes(*value)
    }
}

const fn n_bits_mask(n: u32) -> u32 {
    (1 << n) - 1
}

const fn n_bits_mask_u64(n: u32) -> u64 {
    (1 << n) - 1
}

fn encode_span(
    w: &mut Writer,
    span: &Span,
    span_parts_map: &HashMap<CompressedSpanPart, usize>,
    extra_two_bits: u32,
) {
    let span_parts_index = span_parts_map[&CompressedSpanPart::from_span(span)] as u32;
    let offset = u32::from(span.range.start());
    let len = u32::from(span.range.len());
    encode_span_no_map(w, span_parts_index, offset, len, extra_two_bits, false);
}

fn encode_span_no_map(
    w: &mut Writer,
    mut span_parts_index: u32,
    mut offset: u32,
    mut len: u32,
    extra_two_bits: u32,
    force_heavy_encoding: bool,
) {
    let mut first_u32 = extra_two_bits;
    first_u32 |= (span_parts_index & n_bits_mask(4)) << 2;
    span_parts_index >>= 4;
    first_u32 |= (len & n_bits_mask(8)) << (2 + 4);
    len >>= 8;
    first_u32 |= (offset & n_bits_mask(17)) << (2 + 4 + 8 + 1);
    offset >>= 17;

    let extends_to_next = span_parts_index != 0 || len != 0 || offset != 0 || force_heavy_encoding;
    first_u32 |= u32::from(extends_to_next) << (2 + 4 + 8);

    if extends_to_next {
        if span_parts_index <= n_bits_mask(11)
            && len <= n_bits_mask(10)
            && offset <= n_bits_mask(10)
            && !force_heavy_encoding
        {
            let mut second_u32 = span_parts_index;
            second_u32 |= len << 11;
            second_u32 |= offset << (11 + 10);
            second_u32 <<= 1;
            w.u32(second_u32);
        } else {
            assert!(span_parts_index <= n_bits_mask(24), "too big `span_parts_index`");
            let mut u64 = u64::from(span_parts_index);
            u64 |= u64::from(len) << 24;
            u64 |= u64::from(offset) << (24 + 24);
            let third_u32 = u64 as u32;
            let mut second_u32 = (u64 >> u32::BITS) as u32;
            second_u32 <<= 1;
            second_u32 |= 0b1;
            w.u32(third_u32);
            w.u32(second_u32);
        }
    }
    w.u32(first_u32);
}

fn encode_symbol(w: &mut Writer, symbol: &Symbol, tag: u32, symbols_map: &HashMap<Symbol, usize>) {
    let symbol_idx = symbols_map[symbol] as u32;
    if symbol_idx <= n_bits_mask(6) {
        w.u8(((symbol_idx << 2) | tag) as u8);
    } else if symbol_idx <= n_bits_mask(13) {
        w.u8(((symbol_idx >> 6) << 1) as u8);
        w.u8(((symbol_idx << 2) | 0b10 | tag) as u8);
    } else {
        w.u16((symbol_idx >> 13) as u16);
        w.u8((((symbol_idx >> 6) << 1) | 0b1) as u8);
        w.u8(((symbol_idx << 2) | 0b10 | tag) as u8);
    }
}

fn encode(
    w: &mut Writer,
    tt: TokenTree,
    span_parts_map: &HashMap<CompressedSpanPart, usize>,
    byte_size_after: &mut [u32],
    symbols_map: &HashMap<Symbol, usize>,
) {
    let before = w.len();
    match tt {
        TokenTree::Leaf(Leaf::Punct(Punct { char, spacing, span })) => {
            if char.is_ascii() {
                let spacing = spacing as u8;
                let span_extra = 0b1 | (u32::from(spacing) & 0b10);
                let char = ((char as u8) << 1) | (spacing & 0b1);
                w.u8(char);
                encode_span(w, &span, span_parts_map, span_extra);
            } else {
                let mut control_byte = 0b110;
                control_byte |= (spacing as u8) << 3;
                w.u32(u32::from(char));
                w.u8(control_byte);
                encode_span(w, &span, span_parts_map, 0b00);
            }
        }
        TokenTree::Leaf(Leaf::Ident(Ident { sym, span, is_raw })) => {
            encode_symbol(w, &sym, is_raw as u32, symbols_map);
            encode_span(w, &span, span_parts_map, 0b10);
        }
        TokenTree::Leaf(Leaf::Literal(Literal { text_and_suffix, span, kind, suffix_len })) => {
            if matches!(kind, LitKind::Str | LitKind::StrRaw(0 | 1) | LitKind::Integer)
                && suffix_len <= n_bits_mask(4)
            {
                let mut control_byte = match kind {
                    LitKind::Str => 0b0_011,
                    LitKind::StrRaw(0) => 0b0_100,
                    LitKind::StrRaw(1) => 0b1_011,
                    LitKind::Integer => 0b1_100,
                    _ => unreachable!(),
                };
                control_byte |= (suffix_len as u8) << 4;
                encode_symbol(w, &text_and_suffix, 0, symbols_map);
                w.u8(control_byte);
            } else {
                let mut control_byte = 0b111;
                let (kind, raw_count) = match kind {
                    LitKind::Byte => (0, None),
                    LitKind::Char => (1, None),
                    LitKind::Integer => (2, None),
                    LitKind::Float => (3, None),
                    LitKind::Str => (4, None),
                    LitKind::StrRaw(count) => (5, Some(count)),
                    LitKind::ByteStr => (6, None),
                    LitKind::ByteStrRaw(count) => (7, Some(count)),
                    LitKind::CStr => (8, None),
                    LitKind::CStrRaw(count) => (9, Some(count)),
                    LitKind::Err(()) => (10, None),
                };
                control_byte |= kind << 3;
                encode_symbol(w, &text_and_suffix, 0, symbols_map);
                w.u32(suffix_len);
                if let Some(raw_count) = raw_count {
                    w.u8(raw_count);
                }
                w.u8(control_byte);
            }
            encode_span(w, &span, span_parts_map, 0b00);
        }
        TokenTree::Subtree(Subtree { delimiter, len }) => {
            let open_span_parts_index =
                span_parts_map[&CompressedSpanPart::from_span(&delimiter.open)] as u32;
            let close_span_parts_index =
                span_parts_map[&CompressedSpanPart::from_span(&delimiter.close)] as u32;
            let close_span_offset_from_open = delimiter
                .close
                .range
                .start()
                .checked_sub(delimiter.open.range.start())
                .map_or(u32::MAX, u32::from);
            let children_byte_len = byte_size_after[1] - byte_size_after[1 + len as usize];
            if open_span_parts_index == close_span_parts_index
                && len <= n_bits_mask(u8::BITS)
                && children_byte_len <= n_bits_mask(u8::BITS)
                && delimiter.open.range.len() == TextSize::new(1)
                && delimiter.close.range.len() == TextSize::new(1)
                && close_span_offset_from_open <= n_bits_mask(11)
            {
                let span = Span {
                    range: TextRange::at(
                        delimiter.open.range.start(),
                        TextSize::new(close_span_offset_from_open),
                    ),
                    anchor: delimiter.open.anchor,
                    ctx: delimiter.open.ctx,
                };
                let mut control_byte = 0b000;
                control_byte |= (delimiter.kind as u8) << 3;
                control_byte |= ((close_span_offset_from_open >> 8) << (3 + 2)) as u8;
                w.u8(children_byte_len as u8);
                w.u8(len as u8);
                w.u8(control_byte);
                encode_span(w, &span, span_parts_map, 0b00);
            } else if open_span_parts_index == close_span_parts_index
                && len <= n_bits_mask(u8::BITS)
                && children_byte_len <= n_bits_mask(u8::BITS)
                && delimiter.open.range.len() == delimiter.close.range.len()
                && close_span_offset_from_open <= n_bits_mask(3)
            {
                let mut control_byte = 0b001;
                control_byte |= (delimiter.kind as u8) << 3;
                control_byte |= (close_span_offset_from_open << (3 + 2)) as u8;
                w.u8(children_byte_len as u8);
                w.u8(len as u8);
                w.u8(control_byte);
                encode_span(w, &delimiter.open, span_parts_map, 0b00);
            } else if len <= n_bits_mask(u8::BITS)
                && children_byte_len <= n_bits_mask(12)
                && delimiter.open.range.len() == delimiter.close.range.len()
                && close_span_offset_from_open <= n_bits_mask(8)
                && close_span_parts_index <= n_bits_mask(7)
            {
                let mut control_byte = 0b010;
                control_byte |= (delimiter.kind as u8) << 3;
                control_byte |= (close_span_parts_index << (3 + 2)) as u8;
                let mut children_byte_len = children_byte_len << 4;
                children_byte_len |= close_span_parts_index >> 3;
                w.u16(children_byte_len as u16);
                w.u8(len as u8);
                w.u8(close_span_offset_from_open as u8);
                w.u8(control_byte);
                encode_span(w, &delimiter.open, span_parts_map, 0b00);
            } else {
                let mut control_byte = 0b101;
                control_byte |= (delimiter.kind as u8) << 3;
                w.u32(children_byte_len);
                w.u32(len);
                encode_span(w, &delimiter.close, span_parts_map, 0b00);
                w.u8(control_byte);
                encode_span(w, &delimiter.open, span_parts_map, 0b00);
            }
        }
    }
    let element_byte_size = (w.len() - before) as u32;
    byte_size_after[0] = byte_size_after[1] + element_byte_size;
}

fn encode_top_subtree(
    w: &mut Writer,
    top_subtree: Subtree,
    open_span_parts_index: u32,
    byte_size_after: &[u32],
) {
    let Subtree { delimiter, len } = top_subtree;
    let children_byte_len = byte_size_after[1] - byte_size_after[1 + len as usize];
    let mut control_byte = 0b101;
    control_byte |= (delimiter.kind as u8) << 3;
    w.u32(children_byte_len);
    w.u32(len);
    encode_span_no_map(
        w,
        open_span_parts_index + 1,
        delimiter.close.range.start().into(),
        delimiter.close.range.len().into(),
        0b00,
        true,
    );
    w.u8(control_byte);
    encode_span_no_map(
        w,
        open_span_parts_index,
        delimiter.open.range.start().into(),
        delimiter.open.range.len().into(),
        0b00,
        true,
    );
}

/// A token tree in the old encoding.
#[derive(PartialEq, Eq, Hash)]
pub(super) struct VarintTree {
    buffer: Box<[u8]>,
    span_parts: Box<[CompressedSpanPart]>,
    symbols: Box<[Symbol]>,
    len: usize,
}

impl VarintTree {
    pub(super) fn from_serialized(tts: Vec<TokenTree>) -> VarintTree {
        let mut span_frequencies = HashMap::default();
        let mut symbol_frequencies = HashMap::default();
        for tt in &tts[1..] {
            match tt {
                TokenTree::Leaf(leaf) => {
                    match leaf {
                        Leaf::Literal(Literal { text_and_suffix: symbol, .. })
                        | Leaf::Ident(Ident { sym: symbol, .. }) => {
                            *symbol_frequencies.entry(*symbol).or_insert(0) += 1
                        }
                        Leaf::Punct(_) => {}
                    }
                    *span_frequencies.entry(CompressedSpanPart::from_span(leaf.span())).or_insert(0) +=
                        1;
                }
                TokenTree::Subtree(subtree) => {
                    *span_frequencies
                        .entry(CompressedSpanPart::from_span(&subtree.delimiter.open))
                        .or_insert(0) += 1;
                    *span_frequencies
                        .entry(CompressedSpanPart::from_span(&subtree.delimiter.close))
                        .or_insert(0) += 1;
                }
            }
        }
        encode_all(tts, span_frequencies, symbol_frequencies)
    }

    pub(super) fn flat(&self) -> Vec<TokenTree> {
        let mut reader = Reader { bytes: &self.buffer };
        let mut out = Vec::with_capacity(self.len);
        while !reader.bytes.is_empty() {
            out.push(decode(&mut reader, &self.span_parts, &self.symbols));
        }
        assert_eq!(out.len(), self.len);
        out
    }
}

fn encode_all(
    tts: Vec<TokenTree>,
    mut compressed_span_frequencies: HashMap<CompressedSpanPart, usize>,
    mut symbol_frequencies: HashMap<Symbol, usize>,
) -> VarintTree {
    let tts_len = tts.len();
    let mut token_trees = tts.into_iter().enumerate();
    let Some((_, TokenTree::Subtree(top_subtree))) = token_trees.next() else {
        panic!("must always have a top subtree");
    };

    let (span_parts, span_parts_map) = {
        let mut compressed_spans = compressed_span_frequencies
            .keys()
            .copied()
            .chain([
                CompressedSpanPart::from_span(&top_subtree.delimiter.open),
                CompressedSpanPart::from_span(&top_subtree.delimiter.close),
            ])
            .collect::<Box<[_]>>();
        {
            let len = compressed_spans.len();
            let compressed_spans = &mut compressed_spans[..len - 2];
            if compressed_span_frequencies.len() > n_bits_mask(4) as usize {
                radix::sort_by_key(compressed_spans, |span| !(compressed_span_frequencies[span] as u64));
            }
            for (index, span) in compressed_spans.iter().enumerate() {
                *compressed_span_frequencies.get_mut(span).unwrap() = index;
            }
        }
        (compressed_spans, compressed_span_frequencies)
    };

    let (symbols, symbols_map) = {
        let mut symbols = symbol_frequencies.keys().copied().collect::<Box<[_]>>();
        if symbol_frequencies.len() > n_bits_mask(6) as usize {
            radix::sort_by_key(&mut symbols, |symbol| !(symbol_frequencies[symbol] as u64));
        }
        for (index, symbol) in symbols.iter().enumerate() {
            *symbol_frequencies.get_mut(symbol).unwrap() = index;
        }
        (symbols, symbol_frequencies)
    };

    let mut byte_size_after = vec![0u32; tts_len + 1];
    let mut w = Writer { reversed: Vec::new() };
    for (index, tt) in token_trees.rev() {
        encode(&mut w, tt, &span_parts_map, &mut byte_size_after[index..], &symbols_map);
    }
    encode_top_subtree(&mut w, top_subtree, (span_parts.len() - 2) as u32, &byte_size_after);
    VarintTree { buffer: w.finish(), span_parts, len: tts_len, symbols }
}

struct InlineSpanParts {
    span_parts_index: usize,
    text_range: TextRange,
}

fn decode_span(r: &mut Reader<'_>, mut first_u32: u32) -> InlineSpanParts {
    first_u32 >>= 2;
    let mut span_parts_index = first_u32 & n_bits_mask(4);
    let mut len = (first_u32 >> 4) & n_bits_mask(8);
    let extends_to_next = (first_u32 & (1 << (4 + 8))) != 0;
    let mut offset = first_u32 >> (4 + 8 + 1);
    if extends_to_next {
        let mut second_u32 = r.u32();
        let heavy = (second_u32 & 0b1) != 0;
        second_u32 >>= 1;
        if heavy {
            let third_u32 = r.u32();
            let u64 = u64::from(third_u32) | (u64::from(second_u32) << u32::BITS);
            span_parts_index |= ((u64 & n_bits_mask_u64(24)) as u32) << 4;
            len |= (((u64 >> 24) & n_bits_mask_u64(24)) as u32) << 8;
            offset |= ((u64 >> (24 + 24)) as u32) << 17;
        } else {
            span_parts_index |= (second_u32 & n_bits_mask(11)) << 4;
            len |= ((second_u32 >> 11) & n_bits_mask(10)) << 8;
            offset |= (second_u32 >> (11 + 10)) << 17;
        }
    }
    InlineSpanParts {
        span_parts_index: span_parts_index as usize,
        text_range: TextRange::at(TextSize::new(offset), TextSize::new(len)),
    }
}

fn decode_symbol(r: &mut Reader<'_>, first_byte: u8, symbols: &[Symbol]) -> Symbol {
    let mut symbol_idx = u32::from(first_byte) >> 1;
    let extends_to_next = symbol_idx & 0b1 == 0b1;
    symbol_idx >>= 1;
    if extends_to_next {
        let mut second_byte = r.u8();
        let rest_bytes = if second_byte & 0b1 == 0b1 { u32::from(r.u16()) } else { 0 };
        second_byte >>= 1;
        symbol_idx |= u32::from(second_byte) << 6;
        symbol_idx |= rest_bytes << 13;
    }
    symbols[symbol_idx as usize]
}

fn delimiter_kind(bits: u32) -> DelimiterKind {
    match bits {
        0 => DelimiterKind::Parenthesis,
        1 => DelimiterKind::Brace,
        2 => DelimiterKind::Bracket,
        3 => DelimiterKind::Invisible,
        _ => unreachable!("delimiter kind {bits}"),
    }
}

fn spacing(bits: u32) -> Spacing {
    match bits {
        0 => Spacing::Alone,
        1 => Spacing::Joint,
        2 => Spacing::JointHidden,
        _ => unreachable!("spacing {bits}"),
    }
}

fn decode(r: &mut Reader<'_>, compressed: &[CompressedSpanPart], symbols: &[Symbol]) -> TokenTree {
    let span_and_extra = r.u32();
    let inline = decode_span(r, span_and_extra);
    let span = compressed[inline.span_parts_index].recombine(inline.text_range);

    if span_and_extra & 0b1 == 0b1 {
        let char_and_half_spacing = r.u8();
        let spacing =
            spacing((span_and_extra & 0b10) | (u32::from(char_and_half_spacing) & 0b1));
        let char = char::from(char_and_half_spacing >> 1);
        return TokenTree::Leaf(Leaf::Punct(Punct { char, spacing, span }));
    } else if span_and_extra & 0b10 == 0b10 {
        let symbol_first_byte = r.u8();
        let is_raw = match symbol_first_byte & 0b1 {
            0 => IdentIsRaw::No,
            _ => IdentIsRaw::Yes,
        };
        let sym = decode_symbol(r, symbol_first_byte, symbols);
        return TokenTree::Leaf(Leaf::Ident(Ident { sym, span, is_raw }));
    }

    let control_byte = u32::from(r.u8());
    let extra = control_byte >> 3;
    match control_byte & 0b111 {
        0b000 => {
            let kind = delimiter_kind(extra & 0b11);
            let mut open_span = span;
            let mut close_span_offset_from_open = u32::from(span.range.len());
            open_span.range = TextRange::at(open_span.range.start(), TextSize::new(1));
            close_span_offset_from_open |= (extra >> 2) << 8;
            let close_span = Span {
                range: open_span.range + TextSize::new(close_span_offset_from_open),
                anchor: open_span.anchor,
                ctx: open_span.ctx,
            };
            let len = u32::from(r.u8());
            let _children_byte_len = r.u8();
            TokenTree::Subtree(Subtree {
                delimiter: Delimiter { open: open_span, close: close_span, kind },
                len,
            })
        }
        0b001 => {
            let kind = delimiter_kind(extra & 0b11);
            let close_span = Span {
                range: span.range + TextSize::new(extra >> 2),
                anchor: span.anchor,
                ctx: span.ctx,
            };
            let len = u32::from(r.u8());
            let _children_byte_len = r.u8();
            TokenTree::Subtree(Subtree {
                delimiter: Delimiter { open: span, close: close_span, kind },
                len,
            })
        }
        0b010 => {
            let kind = delimiter_kind(extra & 0b11);
            let close_span_offset_from_open = u32::from(r.u8());
            let len = u32::from(r.u8());
            let children_byte_len = u32::from(r.u16());
            let mut close_span_parts_index = extra >> 2;
            close_span_parts_index |= (children_byte_len & 0b1111) << 3;
            let close_span = compressed[close_span_parts_index as usize]
                .recombine(span.range + TextSize::new(close_span_offset_from_open));
            TokenTree::Subtree(Subtree {
                delimiter: Delimiter { open: span, close: close_span, kind },
                len,
            })
        }
        0b101 => {
            let kind = delimiter_kind(extra);
            let close_span_start = r.u32();
            let close = decode_span(r, close_span_start);
            let close_span = compressed[close.span_parts_index].recombine(close.text_range);
            let len = r.u32();
            let _children_byte_len = r.u32();
            TokenTree::Subtree(Subtree {
                delimiter: Delimiter { open: span, close: close_span, kind },
                len,
            })
        }
        0b110 => {
            let spacing = spacing(extra);
            let char = char::from_u32(r.u32()).expect("a char");
            TokenTree::Leaf(Leaf::Punct(Punct { char, spacing, span }))
        }
        0b011 | 0b100 => {
            let extra = control_byte >> 2;
            let first_byte = r.u8();
            let text_and_suffix = decode_symbol(r, first_byte, symbols);
            let kind = match extra & 0b11 {
                0b00 => LitKind::Str,
                0b01 => LitKind::StrRaw(0),
                0b10 => LitKind::StrRaw(1),
                _ => LitKind::Integer,
            };
            let suffix_len = extra >> 2;
            TokenTree::Leaf(Leaf::Literal(Literal { text_and_suffix, span, kind, suffix_len }))
        }
        0b111 => {
            let kind = match extra {
                0 => LitKind::Byte,
                1 => LitKind::Char,
                2 => LitKind::Integer,
                3 => LitKind::Float,
                4 => LitKind::Str,
                5 => LitKind::StrRaw(r.u8()),
                6 => LitKind::ByteStr,
                7 => LitKind::ByteStrRaw(r.u8()),
                8 => LitKind::CStr,
                9 => LitKind::CStrRaw(r.u8()),
                10 => LitKind::Err(()),
                _ => unreachable!("literal kind {extra}"),
            };
            let suffix_len = r.u32();
            let first_byte = r.u8();
            let text_and_suffix = decode_symbol(r, first_byte, symbols);
            TokenTree::Leaf(Leaf::Literal(Literal { text_and_suffix, span, kind, suffix_len }))
        }
        bits => unreachable!("control bits {bits:#b}"),
    }
}
