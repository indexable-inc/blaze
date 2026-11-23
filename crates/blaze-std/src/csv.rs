//! Strict, bounded, zero-dependency RFC 4180 CSV / TSV reader and writer.
//!
//! - Borrows unquoted fields directly from the input slice with zero string copying
//!   ([`Field::Borrowed`]); only fields containing escaped quotes (`""`) allocate in the caller's
//!   allocator ([`Field::Owned`]).
//! - Zero panicking slice indexes, zero integer `as` casts, zero `.clone()`.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

/// Whether every row must have the same number of columns as the first row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowShape {
    /// Every row must match the first row's column count.
    Rectangular,
    /// Rows may have differing column counts.
    Ragged,
}

/// Bounds enforced while parsing untrusted CSV/TSV text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CsvLimits {
    pub most_bytes: usize,
    pub most_rows: usize,
    pub most_fields: usize,
}

impl CsvLimits {
    /// Generous default bounds (16 MiB, 1,000,000 rows, 4,000,000 fields).
    pub const USUAL: Self = Self {
        most_bytes: 16 << 20,
        most_rows: 1_000_000,
        most_fields: 4_000_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Options for [`parse_in`].
#[derive(Clone, Copy, Debug)]
pub struct CsvOpts<A: Allocator> {
    /// Field separator byte (`b','` for CSV, `b'\t'` for TSV). Must be ASCII and not `"`, `\r`, `\n`.
    pub delimiter: u8,
    /// Whether rows must all share the first row's column count.
    pub shape: RowShape,
    /// Explicit size/row/field bounds.
    pub limits: CsvLimits,
    /// Allocator for parsed rows and any unescaped fields.
    pub alloc: A,
}

/// A parsed CSV field: borrowed zero-copy when no `""` unescaping was needed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Field<'a, A: Allocator> {
    Borrowed(&'a str),
    Owned(String<A>),
}

impl<A: Allocator> Field<'_, A> {
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Field::Borrowed(s) => s,
            Field::Owned(s) => s.as_str(),
        }
    }
}

/// A parsed CSV table stored flat in two arrays (all fields end-to-end + row end offsets).
#[derive(Debug)]
pub struct Table<'a, A: Allocator> {
    fields: Vec<Field<'a, A>, A>,
    row_ends: Vec<usize, A>,
}

impl<'a, A: Allocator> Table<'a, A> {
    /// Number of rows in the table.
    #[inline]
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_ends.len()
    }

    /// True when the table has zero rows.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.row_ends.is_empty()
    }

    /// Return the slice of fields in 0-based `row`.
    #[inline]
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[Field<'a, A>]> {
        let end = *self.row_ends.get(row)?;
        let start = if row == 0 { 0 } else { self.row_ends.get(row - 1).copied().unwrap_or(0) };
        self.fields.as_slice().get(start..end)
    }

    /// Iterate over all rows as slices of [`Field`].
    pub fn rows(&self) -> impl Iterator<Item = &[Field<'a, A>]> {
        let mut start = 0usize;
        self.row_ends.as_slice().iter().map(move |&end| {
            let slice = self.fields.as_slice().get(start..end).unwrap_or(&[]);
            start = end;
            slice
        })
    }
}

/// Why parsing CSV/TSV failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CsvError {
    /// 1-based row number.
    pub row: usize,
    /// 1-based column number.
    pub col: usize,
    /// Byte offset in the input text.
    pub offset: usize,
    /// Specific failure reason.
    pub why: CsvWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CsvWhy {
    InvalidDelimiter { byte: u8 },
    InputTooLarge { bytes: usize, limit: usize },
    TooManyRows { limit: usize },
    TooManyFields { limit: usize },
    UnclosedQuote,
    UnexpectedCharAfterQuote { byte: u8 },
    QuoteInUnquotedField,
    RaggedRow { expected_cols: usize, actual_cols: usize },
    Alloc(AllocFailure),
}

impl fmt::Display for CsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::csv: row {}, col {} (byte {}): ", self.row, self.col, self.offset)?;
        match self.why {
            CsvWhy::InvalidDelimiter { byte } => {
                write!(f, "invalid delimiter byte 0x{byte:02x}")
            }
            CsvWhy::InputTooLarge { bytes, limit } => {
                write!(f, "input size {bytes} bytes exceeds limit {limit}")
            }
            CsvWhy::TooManyRows { limit } => write!(f, "row count exceeds limit {limit}"),
            CsvWhy::TooManyFields { limit } => write!(f, "field count exceeds limit {limit}"),
            CsvWhy::UnclosedQuote => f.write_str("unterminated quoted field"),
            CsvWhy::UnexpectedCharAfterQuote { byte } => {
                write!(f, "unexpected byte 0x{byte:02x} after closing quote")
            }
            CsvWhy::QuoteInUnquotedField => f.write_str("unexpected `\"` inside unquoted field"),
            CsvWhy::RaggedRow { expected_cols, actual_cols } => {
                write!(f, "expected {expected_cols} columns, found {actual_cols}")
            }
            CsvWhy::Alloc(err) => write!(f, "allocation failed ({err})"),
        }
    }
}

impl core::error::Error for CsvError {}

/// Parse RFC 4180 CSV/TSV text into a flat [`Table`] allocated in `opts.alloc`.
pub fn parse_in<'a, A: Allocator + Copy>(
    text: &'a str,
    opts: CsvOpts<A>,
) -> Result<Table<'a, A>, CsvError> {
    if matches!(opts.delimiter, b'"' | b'\r' | b'\n') || opts.delimiter >= 0x80 {
        return Err(CsvError {
            row: 1,
            col: 1,
            offset: 0,
            why: CsvWhy::InvalidDelimiter { byte: opts.delimiter },
        });
    }
    if text.len() > opts.limits.most_bytes {
        return Err(CsvError {
            row: 1,
            col: 1,
            offset: 0,
            why: CsvWhy::InputTooLarge { bytes: text.len(), limit: opts.limits.most_bytes },
        });
    }

    let mut fields = Vec::new_in(opts.alloc);
    let mut row_ends = Vec::new_in(opts.alloc);
    if text.is_empty() {
        return Ok(Table { fields, row_ends });
    }

    let bytes = text.as_bytes();
    let delim = opts.delimiter;
    let mut i = 0usize;
    let mut row_num = 1usize;
    let mut expected_cols: Option<usize> = None;

    while i < bytes.len() {
        if row_num > opts.limits.most_rows {
            return Err(CsvError {
                row: row_num,
                col: 1,
                offset: i,
                why: CsvWhy::TooManyRows { limit: opts.limits.most_rows },
            });
        }
        let row_start_fields = fields.len();
        let mut col_num = 1usize;

        loop {
            if fields.len() >= opts.limits.most_fields {
                return Err(CsvError {
                    row: row_num,
                    col: col_num,
                    offset: i,
                    why: CsvWhy::TooManyFields { limit: opts.limits.most_fields },
                });
            }

            let field = if bytes.get(i) == Some(&b'"') {
                parse_quoted(text, bytes, &mut i, delim, row_num, col_num, opts.alloc)?
            } else {
                parse_unquoted(text, bytes, &mut i, delim, row_num, col_num)?
            };
            fields.try_push(field).map_err(|r| CsvError {
                row: row_num,
                col: col_num,
                offset: i,
                why: CsvWhy::Alloc(r.why),
            })?;

            match bytes.get(i) {
                None => break,
                Some(&b) if b == delim => {
                    i += 1;
                    col_num += 1;
                    continue;
                }
                Some(&b'\r') if bytes.get(i + 1) == Some(&b'\n') => {
                    i += 2;
                    break;
                }
                Some(&b'\n') => {
                    i += 1;
                    break;
                }
                Some(_) => break,
            }
        }

        let actual_cols = fields.len().saturating_sub(row_start_fields);
        if opts.shape == RowShape::Rectangular {
            match expected_cols {
                None => expected_cols = Some(actual_cols),
                Some(exp) if exp != actual_cols => {
                    return Err(CsvError {
                        row: row_num,
                        col: col_num,
                        offset: i,
                        why: CsvWhy::RaggedRow { expected_cols: exp, actual_cols },
                    });
                }
                Some(_) => {}
            }
        }
        row_ends.try_push(fields.len()).map_err(|r| CsvError {
            row: row_num,
            col: col_num,
            offset: i,
            why: CsvWhy::Alloc(r.why),
        })?;
        row_num += 1;
    }

    Ok(Table { fields, row_ends })
}

fn parse_unquoted<'a, A: Allocator>(
    text: &'a str,
    bytes: &[u8],
    i: &mut usize,
    delim: u8,
    row: usize,
    col: usize,
) -> Result<Field<'a, A>, CsvError> {
    let start = *i;
    while let Some(&b) = bytes.get(*i) {
        if b == delim || b == b'\n' {
            break;
        }
        if b == b'\r' && bytes.get(*i + 1) == Some(&b'\n') {
            break;
        }
        if b == b'"' {
            return Err(CsvError { row, col, offset: *i, why: CsvWhy::QuoteInUnquotedField });
        }
        *i += 1;
    }
    Ok(Field::Borrowed(text.get(start..*i).unwrap_or("")))
}

fn parse_quoted<'a, A: Allocator>(
    text: &'a str,
    bytes: &[u8],
    i: &mut usize,
    delim: u8,
    row: usize,
    col: usize,
    alloc: A,
) -> Result<Field<'a, A>, CsvError> {
    let open_pos = *i;
    *i += 1; // skip opening '"'
    let content_start = *i;
    let mut has_escaped = false;

    while let Some(&b) = bytes.get(*i) {
        if b == b'"' {
            if bytes.get(*i + 1) == Some(&b'"') {
                has_escaped = true;
                *i += 2;
                continue;
            }
            let content_end = *i;
            *i += 1; // skip closing '"'
            if let Some(&next) = bytes.get(*i) {
                let ok = next == delim
                    || next == b'\n'
                    || (next == b'\r' && bytes.get(*i + 1) == Some(&b'\n'));
                if !ok {
                    return Err(CsvError {
                        row,
                        col,
                        offset: *i,
                        why: CsvWhy::UnexpectedCharAfterQuote { byte: next },
                    });
                }
            }
            let raw = text.get(content_start..content_end).unwrap_or("");
            if !has_escaped {
                return Ok(Field::Borrowed(raw));
            }
            let mut unescaped = String::try_with_capacity_in(raw.len(), alloc)
                .map_err(|e| CsvError { row, col, offset: open_pos, why: CsvWhy::Alloc(e) })?;
            let raw_bytes = raw.as_bytes();
            let mut seg_start = 0usize;
            let mut j = 0usize;
            while let Some(&rb) = raw_bytes.get(j) {
                if rb == b'"' && raw_bytes.get(j + 1) == Some(&b'"') {
                    if let Some(seg) = raw.get(seg_start..=j) {
                        unescaped.try_push_str(seg).map_err(|e| CsvError {
                            row,
                            col,
                            offset: content_start + j,
                            why: CsvWhy::Alloc(e),
                        })?;
                    }
                    j += 2;
                    seg_start = j;
                    continue;
                }
                j += 1;
            }
            if let Some(tail) = raw.get(seg_start..) {
                unescaped.try_push_str(tail).map_err(|e| CsvError {
                    row,
                    col,
                    offset: content_end,
                    why: CsvWhy::Alloc(e),
                })?;
            }
            return Ok(Field::Owned(unescaped));
        }
        *i += 1;
    }

    Err(CsvError { row, col, offset: open_pos, why: CsvWhy::UnclosedQuote })
}

/// Write a single CSV/TSV row into `out`, minimally quoting fields containing the delimiter,
/// quotes, `\r`, or `\n`.
pub fn write_row_into<W: fmt::Write, S: AsRef<str>>(
    fields: &[S],
    delimiter: u8,
    crlf: bool,
    out: &mut W,
) -> fmt::Result {
    write_row_into_planted(fields, delimiter, crlf, out, false)
}

fn write_row_into_planted<W: fmt::Write, S: AsRef<str>>(
    fields: &[S],
    delimiter: u8,
    crlf: bool,
    out: &mut W,
    plant_skip_quote_escape: bool,
) -> fmt::Result {
    let delim_char = char::from(delimiter);
    for (idx, field) in fields.iter().enumerate() {
        if idx > 0 {
            out.write_char(delim_char)?;
        }
        let s = field.as_ref();
        let needs_quote = (fields.len() == 1 && s.is_empty())
            || s.as_bytes()
                .iter()
                .any(|&b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r');
        if needs_quote {
            out.write_char('"')?;
            for ch in s.chars() {
                if ch == '"' && !plant_skip_quote_escape {
                    out.write_str("\"\"")?;
                } else {
                    out.write_char(ch)?;
                }
            }
            out.write_char('"')?;
        } else {
            out.write_str(s)?;
        }
    }
    if crlf {
        out.write_str("\r\n")
    } else {
        out.write_char('\n')
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;

    #[test]
    fn csv_and_tsv_basics_and_errors() {
        let h = heap();
        let data = "name,note\r\n\"alice\",\"hello, \"\"world\"\"\nline2\"\r\nbob,plain\r\n";
        let table = parse_in(
            data,
            CsvOpts {
                delimiter: b',',
                shape: RowShape::Rectangular,
                limits: CsvLimits::USUAL,
                alloc: h.alloc(),
            },
        )
        .expect("parse csv");
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.row(0).expect("r0")[0].as_str(), "name");
        assert_eq!(table.row(1).expect("r1")[1].as_str(), "hello, \"world\"\nline2");
        assert!(matches!(table.row(2).expect("r2")[1], Field::Borrowed("plain")));

        let ragged = parse_in(
            "a,b\n1,2,3\n",
            CsvOpts {
                delimiter: b',',
                shape: RowShape::Rectangular,
                limits: CsvLimits::USUAL,
                alloc: h.alloc(),
            },
        );
        assert!(matches!(
            ragged,
            Err(CsvError { row: 2, why: CsvWhy::RaggedRow { expected_cols: 2, actual_cols: 3 }, .. })
        ));
    }

    fn check_csv_round_trip<A: Allocator + Copy>(
        rows: &[OwnVec<OwnString<A>, A>],
        delimiter: u8,
        crlf: bool,
        plant_skip_quote_escape: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut encoded = OwnString::new_in(alloc);
        for row in rows {
            write_row_into_planted(
                row.as_slice(),
                delimiter,
                crlf,
                &mut encoded,
                plant_skip_quote_escape,
            )
            .expect("write");
        }
        let table = parse_in(
            encoded.as_str(),
            CsvOpts { delimiter, shape: RowShape::Ragged, limits: CsvLimits::USUAL, alloc },
        )
        .map_err(|e| Broke::Oracle(format_in!(alloc, "parse failed: {e}").as_str().into()))?;
        if table.row_count() != rows.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "row count mismatch: expected {}, got {}", rows.len(), table.row_count())
                    .as_str()
                    .into(),
            ));
        }
        for (r_idx, (got_row, exp_row)) in table.rows().zip(rows.iter()).enumerate() {
            if got_row.len() != exp_row.len() {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "row {r_idx} col count mismatch: expected {}, got {}",
                        exp_row.len(),
                        got_row.len()
                    )
                    .as_str()
                    .into(),
                ));
            }
            for (c_idx, (got, exp)) in got_row.iter().zip(exp_row.as_slice().iter()).enumerate() {
                if got.as_str() != exp.as_str() {
                    return Err(Broke::Oracle(
                        format_in!(
                            alloc,
                            "row {r_idx} col {c_idx}: expected {:?}, got {:?}",
                            exp.as_str(),
                            got.as_str()
                        )
                        .as_str()
                        .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn draw_rows<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        alloc: A,
    ) -> OwnVec<OwnVec<OwnString<A>, A>, A> {
        const PIECES: &[&str] = &["", "abc", ",", "\t", "\"", "\n", "\r\n", "café", "x\"y"];
        let num_rows = usize::try_from(tape.int_in(0..=8, "rows")).unwrap_or(0);
        let mut rows = OwnVec::new_in(alloc);
        for _ in 0..num_rows {
            let num_cols = usize::try_from(tape.int_in(1..=6, "cols")).unwrap_or(1);
            let mut row = OwnVec::new_in(alloc);
            for _ in 0..num_cols {
                let parts = usize::try_from(tape.int_in(0..=3, "parts")).unwrap_or(0);
                let mut cell = OwnString::new_in(alloc);
                for _ in 0..parts {
                    let pick = usize::try_from(tape.below(PIECES.len() as u64, "piece")).unwrap_or(0);
                    cell.push_str(PIECES[pick]);
                }
                row.push(cell);
            }
            rows.push(row);
        }
        rows
    }

    /// The invariant: CSV formatting and parsing round-trip arbitrary cell values without loss.
    #[test]
    fn property_csv_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let rows = draw_rows(tape, h.alloc());
            let delimiter = if tape.coin("tsv") { b'\t' } else { b',' };
            let crlf = tape.coin("crlf");
            check_csv_round_trip(rows.as_slice(), delimiter, crlf, false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_unescaped_quotes() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let rows = draw_rows(tape, h.alloc());
            check_csv_round_trip(rows.as_slice(), b',', false, plant, h.alloc())
        });
    }
}
