//! FSST (Boncz, Neumann and Leis, "FSST: Fast Random Access String Compression", VLDB 2020):
//! a static table of up to 255 symbols of 1 to 8 bytes, built once a column from a sample;
//! a string is the codes of the symbols it is cut into, longest first, with code 255 escaping
//! one literal byte. Any one string decodes alone, from its codes and the table: random access.
//!
//! # Building the table
//!
//! [`ROUNDS`] rounds over a sample of at most [`SAMPLE`] bytes, as the paper's: each round cuts
//! the sample with the table so far, counts every symbol it used (and the single byte under
//! each longer symbol, so bytes keep competing) and every pair of symbols cut one after the
//! other; each counted symbol and each pair's concatenation (cut to 8 bytes) is a candidate
//! of gain `count * length`; the 255 of most gain are the next table. Ties go to the shorter,
//! then the lesser bytes, so a sample has one table (rule 25). The paper's encoder finds the
//! longest match through a lossy perfect hash of 3-byte prefixes; this one walks the table's
//! symbols that start with the byte, longest first: the same cut, slower to encode. Unsure:
//! the paper's sample grows from round to round (its later rounds see more of it) and it
//! weighs a single byte's gain differently; this is the simplest form, the whole sample each
//! round and `count * length` for all.
//!
//! # Layout
//!
//! The table: its symbol count `n` (1 byte), the `n` lengths (1 byte each), the `n` symbols'
//! bytes end to end. Code `c` below `n` is symbol `c`; [`ESCAPE`] is followed by one literal
//! byte; any other code is a corrupt column.

use blaze_int::Int;
use blaze_own::{AVec, Alloc, Vec};

use super::bits;
use super::plant::{self, Plant};
use super::{EncError, put, read8};

/// The most symbols a table holds; code 255 is the escape.
pub const MAX_SYMBOLS: usize = 255;

/// The code before a literal byte.
pub const ESCAPE: u8 = 255;

/// Rounds of the table's construction.
pub const ROUNDS: usize = 5;

/// The most bytes of sample a table is built from.
pub const SAMPLE: usize = 16 * 1024;

/// A pseudo-code for a byte no symbol is: `256 + byte`. Codes 0..256 are the table's.
const CODES: usize = 512;

/// A symbol table, ready to encode with.
#[derive(Clone, Debug)]
pub struct Table {
    /// Symbol `c`'s bytes, little-endian in a `u64`, zero past its length.
    symbols: [u64; MAX_SYMBOLS],
    lens: [u8; MAX_SYMBOLS],
    n: usize,
    /// The codes in order of first byte, longest first, then lesser bytes.
    order: [u8; MAX_SYMBOLS],
    /// The symbols starting with byte `b` are `order[first[b]..first[b + 1]]`.
    first: [u16; 257],
}

/// Up to 8 bytes of `s` from `at`, little-endian, and how many.
#[inline(always)]
fn window(s: &[u8], at: usize) -> (u64, usize) {
    match s.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        Some(b) => (u64::from_le_bytes(*b), 8),
        None => {
            let rest = &s[at.min(s.len())..];
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            (u64::from_le_bytes(b), rest.len())
        }
    }
}

impl Table {
    /// The table of no symbol: every byte escaped.
    pub fn empty() -> Table {
        Table { symbols: [0; MAX_SYMBOLS], lens: [0; MAX_SYMBOLS], n: 0, order: [0; MAX_SYMBOLS], first: [0; 257] }
    }

    /// The table of `symbols` (bytes and length, at most 255), indexed for encoding.
    fn of(symbols: &[(u64, u8)]) -> Table {
        let mut table = Table::empty();
        table.n = symbols.len().min(MAX_SYMBOLS);
        for (c, &(bytes, len)) in symbols.iter().take(MAX_SYMBOLS).enumerate() {
            table.symbols[c] = bytes;
            table.lens[c] = len;
            table.order[c] = c.truncate_to::<u8>();
        }
        let (symbols, lens) = (table.symbols, table.lens);
        table.order[..table.n].sort_unstable_by_key(|&c| {
            let c = usize::from(c);
            (symbols[c].truncate_to::<u8>(), u8::MAX - lens[c], symbols[c])
        });
        let mut at = 0usize;
        for b in 0..256usize {
            table.first[b] = at.truncate_to::<u16>();
            while at < table.n && usize::from(symbols[usize::from(table.order[at])].truncate_to::<u8>()) == b {
                at += 1;
            }
        }
        table.first[256] = at.truncate_to::<u16>();
        table
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The longest symbol `s` starts with at `at`: its code and length, or `None`.
    #[inline]
    fn longest(&self, s: &[u8], at: usize) -> Option<(u8, usize)> {
        let (bytes, have) = window(s, at);
        let b = usize::from(s[at]);
        for &code in &self.order[usize::from(self.first[b])..usize::from(self.first[b + 1])] {
            let len = usize::from(self.lens[usize::from(code)]);
            if len <= have && (bytes & bits::mask((8 * len).truncate_to::<u8>())) == self.symbols[usize::from(code)] {
                return Some((code, len));
            }
        }
        None
    }

    /// The table built from `sample` (strings, of which at most [`SAMPLE`] bytes are read).
    pub fn build(sample: &[&[u8]], alloc: Alloc<'_>) -> Result<Table, EncError> {
        let mut table = Table::empty();
        let mut single: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(CODES, alloc)?;
        let mut pairs: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(CODES * CODES, alloc)?;
        let mut candidates: Vec<(u64, u8, u64), Alloc<'_>> = Vec::new_in(alloc);
        for _ in 0..ROUNDS {
            single.clear();
            single.resize(CODES, 0);
            pairs.clear();
            pairs.resize(CODES * CODES, 0);
            let mut read = 0usize;
            for s in sample {
                if read >= SAMPLE {
                    break;
                }
                let s = &s[..s.len().min(SAMPLE - read)];
                read += s.len();
                let (mut at, mut prev) = (0usize, None::<usize>);
                while at < s.len() {
                    let (code, len) = match table.longest(s, at) {
                        Some((code, len)) => (usize::from(code), len),
                        None => (256 + usize::from(s[at]), 1),
                    };
                    single[code] += 1;
                    if len > 1 {
                        single[256 + usize::from(s[at])] += 1;
                    }
                    if let Some(p) = prev {
                        pairs[p * CODES + code] += 1;
                    }
                    prev = Some(code);
                    at += len;
                }
            }
            let symbol = |code: usize| -> (u64, u8) {
                if code >= 256 { ((code - 256).widen_to::<u64>(), 1) } else { (table.symbols[code], table.lens[code]) }
            };
            candidates.clear();
            for code in 0..CODES {
                let count = single[code];
                if count > 0 {
                    let (bytes, len) = symbol(code);
                    candidates.try_reserve(1)?;
                    candidates.push((bytes, len, u64::from(count) * u64::from(len)));
                }
            }
            for a in 0..CODES {
                if single[a] == 0 {
                    continue;
                }
                let (ab, al) = symbol(a);
                if al >= 8 {
                    continue;
                }
                for b in 0..CODES {
                    let count = pairs[a * CODES + b];
                    if count == 0 {
                        continue;
                    }
                    let (bb, bl) = symbol(b);
                    let len = (al + bl).min(8);
                    let bytes = (ab | (bb << (8 * u32::from(al)))) & bits::mask(8 * len);
                    candidates.try_reserve(1)?;
                    candidates.push((bytes, len, u64::from(count) * u64::from(len)));
                }
            }
            // One candidate a symbol, its gains summed.
            candidates.sort_unstable_by_key(|&(bytes, len, _)| (len, bytes));
            let mut kept = 0usize;
            for k in 0..candidates.len() {
                if kept > 0 && (candidates[kept - 1].0, candidates[kept - 1].1) == (candidates[k].0, candidates[k].1) {
                    candidates[kept - 1].2 += candidates[k].2;
                } else {
                    candidates[kept] = candidates[k];
                    kept += 1;
                }
            }
            candidates.truncate(kept);
            candidates.sort_unstable_by_key(|&(bytes, len, gain)| (u64::MAX - gain, len, bytes));
            let mut chosen = [(0u64, 0u8); MAX_SYMBOLS];
            let n = candidates.len().min(MAX_SYMBOLS);
            for (slot, &(bytes, len, _)) in chosen.iter_mut().zip(candidates.iter()) {
                *slot = (bytes, len);
            }
            table = Table::of(&chosen[..n]);
        }
        Ok(table)
    }

    /// Appends `s` cut into the table's codes.
    pub fn encode(&self, s: &[u8], out: &mut AVec<'_, u8>) -> Result<(), EncError> {
        out.try_reserve(2 * s.len())?;
        let mut at = 0;
        while at < s.len() {
            match self.longest(s, at) {
                Some((code, len)) => {
                    out.push(code);
                    at += len;
                }
                None => {
                    out.push(ESCAPE);
                    if !plant::on(Plant::EscapeDropsByte) {
                        out.push(s[at]);
                    }
                    at += 1;
                }
            }
        }
        Ok(())
    }

    /// Bytes [`Table::write`] takes.
    pub fn size(&self) -> usize {
        1 + self.n + self.lens[..self.n].iter().map(|&l| usize::from(l)).sum::<usize>()
    }

    /// Appends the table (see the module's layout).
    pub fn write(&self, out: &mut AVec<'_, u8>) -> Result<(), EncError> {
        out.try_reserve(self.size())?;
        put(out, &[self.n.truncate_to::<u8>()])?;
        put(out, &self.lens[..self.n])?;
        for c in 0..self.n {
            put(out, &self.symbols[c].to_le_bytes()[..usize::from(self.lens[c])])?;
        }
        Ok(())
    }
}

/// A table read from a column: what decoding needs, made once by [`Decoder::open`].
#[derive(Clone, Debug)]
pub struct Decoder {
    symbols: [u64; 256],
    /// Zero for a code no symbol is (the escape's too).
    lens: [u8; 256],
}

impl Decoder {
    /// The table at the start of `bytes`, and the bytes it takes; `at` names its place.
    pub fn open(bytes: &[u8], at: usize) -> Result<(Decoder, usize), EncError> {
        let n = usize::from(read8(bytes, 0, "a symbol table's count")?);
        if n > MAX_SYMBOLS {
            return Err(EncError::Corrupt { what: "a symbol table of 256 symbols", at });
        }
        let lens = bytes.get(1..1 + n).ok_or(EncError::Corrupt { what: "a symbol table's lengths", at })?;
        let mut decoder = Decoder { symbols: [0; 256], lens: [0; 256] };
        let mut from = 1 + n;
        for (c, &len) in lens.iter().enumerate() {
            if len == 0 || len > 8 {
                return Err(EncError::Corrupt { what: "a symbol of no or more than 8 bytes", at: at + 1 + c });
            }
            let symbol = bytes
                .get(from..from + usize::from(len))
                .ok_or(EncError::Corrupt { what: "a symbol past the table's end", at: at + from })?;
            let mut b = [0u8; 8];
            b[..symbol.len()].copy_from_slice(symbol);
            decoder.symbols[c] = u64::from_le_bytes(b);
            decoder.lens[c] = len;
            from += usize::from(len);
        }
        Ok((decoder, from))
    }

    /// The most bytes `codes` can decode to: 8 a code.
    pub fn bound(codes: &[u8]) -> usize {
        8 * codes.len()
    }

    /// `codes` decoded into the start of `buf` (at least [`Decoder::bound`] long); the bytes
    /// written. `index` names the string in an error.
    #[inline]
    pub fn decode(&self, codes: &[u8], buf: &mut [u8], index: usize) -> Result<usize, EncError> {
        let need = Decoder::bound(codes);
        if buf.len() < need {
            return Err(EncError::Buffer { index, need, have: buf.len() });
        }
        let (mut at, mut o) = (0usize, 0usize);
        while at < codes.len() {
            let code = codes[at];
            if code == ESCAPE {
                let Some(&byte) = codes.get(at + 1) else {
                    return Err(EncError::Corrupt { what: "an escape at a string's end", at });
                };
                buf[o] = byte;
                o += 1;
                at += 2;
                continue;
            }
            let len = usize::from(self.lens[usize::from(code)]);
            if len == 0 {
                return Err(EncError::Corrupt { what: "a code no symbol is", at });
            }
            // 8 bytes a code are in `buf`: the whole word is written, `len` of it kept.
            buf[o..o + 8].copy_from_slice(&self.symbols[usize::from(code)].to_le_bytes());
            o += len;
            at += 1;
        }
        Ok(o)
    }
}
