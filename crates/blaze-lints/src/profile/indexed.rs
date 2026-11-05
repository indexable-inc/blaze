//! A reader of LLVM's indexed instrumentation profile, the `.profdata` `llvm-profdata merge`
//! writes (`llvm/lib/ProfileData/InstrProfReader.cpp`'s `IndexedInstrProfReader`, formats 4 to
//! 12). It reads what the profile lints weigh a function by: each record's name, its control
//! flow hash and its counters. The summary, value profiles, MC/DC bitmaps, memory profiles,
//! binary IDs, temporal traces and vtable names are stepped over.
//!
//! The reader is blaze's own rather than LLVM's through a C++ shim: `blaze check` runs in every
//! blaze, the ones built without LLVM (`blaze-engine/llvm` off) too, and the format the lints
//! read is a few dozen lines of it. Any byte it does not expect is an error naming where it
//! was, never a record left out.
//!
//! The layout (all integers little-endian): a header of `u64`s (magic, version and its
//! variant flags, an unused word, the hash type, the hash table's offset, then one offset per
//! later section by version); the profile summary (twice for a context-sensitive profile);
//! then the payload of LLVM's `OnDiskIterableChainedHashTable`, bucket by bucket: a `u16` count
//! of the bucket's entries, each a `u64` key hash, a `u64` key length, a `u64` data length, the
//! key (the function's PGO name) and the data (its records, one per control flow hash). The
//! table's bucket array at the hash offset starts with the bucket count and the entry count.

/// `IndexedInstrProf::Magic`: `\xfflprofi\x81`.
pub(crate) const MAGIC: u64 = 0x8169_666f_7270_6cff;

/// `VARIANT_MASKS_ALL`: the version's high half holds the kind's flags.
const VARIANT_MASKS: u64 = 0xffff_ffff_0000_0000;
/// `VARIANT_MASK_IR_PROF`: counted by LLVM's IR instrumentation, not clang's front end.
pub(crate) const IR_PROF: u64 = 1 << 56;
/// `VARIANT_MASK_CSIR_PROF`: a context-sensitive profile, which has a second summary.
pub(crate) const CSIR_PROF: u64 = 1 << 57;
/// `VARIANT_MASK_INSTR_ENTRY`: each function's entry block is counted, by its first counter.
pub(crate) const INSTR_ENTRY: u64 = 1 << 58;
/// `VARIANT_MASK_BYTE_COVERAGE`: one byte per block, set or not; no counts.
const BYTE_COVERAGE: u64 = 1 << 60;
/// `VARIANT_MASK_FUNCTION_ENTRY_ONLY`: each function's one counter is its entry's.
pub(crate) const FUNCTION_ENTRY_ONLY: u64 = 1 << 61;

/// The formats read: 4, the first with a summary, to 12, LLVM 19's and later's.
const VERSIONS: std::ops::RangeInclusive<u64> = 4..=12;

/// Why a profile does not read: a description of the bytes at a place, which the caller puts
/// after the profile's path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// The first eight bytes are not the indexed profile's magic: a raw profile
    /// (`.profraw`), a text one, or no profile at all.
    NotIndexed,
    /// A format this reader does not know, with the version word as written.
    Version(u64),
    /// A kind of profile with nothing to weigh a function by.
    Kind(&'static str),
    /// The bytes end, or a length points past them, where `what` was to be read.
    Truncated { at: usize, what: &'static str },
    /// A value that the format does not allow.
    Malformed { at: usize, what: &'static str },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotIndexed => f.write_str(
                "is not an indexed profile (its magic is not `\\xfflprofi\\x81`): merge the raw profiles with `llvm-profdata merge -o <file>.profdata`",
            ),
            ReadError::Version(word) => write!(
                f,
                "is indexed profile format {}, and this reader takes formats {} to {}",
                word & !VARIANT_MASKS,
                VERSIONS.start(),
                VERSIONS.end()
            ),
            ReadError::Kind(what) => write!(f, "is {what}, which has no counts to weigh a function by"),
            ReadError::Truncated { at, what } => write!(f, "ends at byte {at}, where {what} was to be read"),
            ReadError::Malformed { at, what } => write!(f, "has {what} at byte {at}"),
        }
    }
}

/// What the header says of the whole profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The format, without its flags.
    pub version: u32,
    /// The variant flags (`VARIANT_MASK_IR_PROF`, `VARIANT_MASK_INSTR_ENTRY`, ...).
    pub flags: u64,
}

impl Header {
    /// Whether a record's first counter counts its function's entries: a front-end profile's
    /// (its first counter is the function's body), or an IR one made with entry counters.
    pub fn entry_counted(self) -> bool {
        self.flags & IR_PROF == 0 || self.flags & (INSTR_ENTRY | FUNCTION_ENTRY_ONLY) != 0
    }
}

/// One record: a function's PGO name and its counters, as the bytes hold them (`u64`s,
/// little-endian). Its control flow hash, which tells a record of one build of the function
/// from another's, is stepped over: the lints sum every record of a name.
pub(crate) struct Record<'a> {
    pub(crate) name: &'a [u8],
    counters: &'a [u8],
}

impl Record<'_> {
    /// Its counters, in the order the instrumentation numbered them.
    pub(crate) fn counts(&self) -> impl Iterator<Item = u64> + '_ {
        self.counters.chunks_exact(8).map(|it| u64::from_le_bytes(it.try_into().unwrap_or([0; 8])))
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: u64, what: &'static str) -> Result<&'a [u8], ReadError> {
        let truncated = ReadError::Truncated { at: self.at, what };
        let len = usize::try_from(len).map_err(|_| truncated.clone())?;
        let end = self.at.checked_add(len).ok_or_else(|| truncated.clone())?;
        let taken = self.bytes.get(self.at..end).ok_or(truncated)?;
        self.at = end;
        Ok(taken)
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, ReadError> {
        let bytes = self.take(8, what)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ReadError::Truncated { at: self.at, what })?,
        ))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, ReadError> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().map_err(|_| ReadError::Truncated { at: self.at, what })?,
        ))
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, ReadError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_le_bytes(
            bytes.try_into().map_err(|_| ReadError::Truncated { at: self.at, what })?,
        ))
    }
}

/// Reads the indexed profile `bytes`, handing each record to `each` in the order the file has
/// them (the hash table's bucket order: the same for the same bytes).
pub(crate) fn read<'a>(
    bytes: &'a [u8],
    mut each: impl FnMut(Record<'a>),
) -> Result<Header, ReadError> {
    let mut cur = Cursor { bytes, at: 0 };
    if cur.u64("the magic").map_err(|_| ReadError::NotIndexed)? != MAGIC {
        return Err(ReadError::NotIndexed);
    }
    let word = cur.u64("the version")?;
    let version = word & !VARIANT_MASKS;
    if !VERSIONS.contains(&version) {
        return Err(ReadError::Version(word));
    }
    let flags = word & VARIANT_MASKS;
    if flags & BYTE_COVERAGE != 0 {
        return Err(ReadError::Kind(
            "a byte coverage profile (`-pgo-instrument-entry` with single-byte counters)",
        ));
    }
    let _unused = cur.u64("the header's unused word")?;
    let _hash_type = cur.u64("the hash type")?;
    let hash_offset = cur.u64("the hash table's offset")?;
    // `Header::size()`: one more offset from format 8 (memory profiles), 9 (binary IDs), 10
    // (temporal traces) and 12 (vtable names).
    let later = match version {
        ..=7 => 0,
        8 => 1,
        9 => 2,
        10 | 11 => 3,
        _ => 4,
    };
    for _ in 0..later {
        cur.u64("a section's offset in the header")?;
    }
    let summaries = if flags & CSIR_PROF != 0 { 2 } else { 1 };
    for _ in 0..summaries {
        let fields = cur.u64("the summary's field count")?;
        let entries = cur.u64("the summary's cutoff count")?;
        let words = entries
            .checked_mul(3)
            .and_then(|it| it.checked_add(fields))
            .and_then(|it| it.checked_mul(8))
            .ok_or(ReadError::Malformed { at: cur.at, what: "a summary larger than any file" })?;
        cur.take(words, "the summary")?;
    }
    let mut table = Cursor { bytes, at: 0 };
    table.take(hash_offset, "the hash table, at the offset the header gives")?;
    let _buckets = table.u64("the hash table's bucket count")?;
    let entries = table.u64("the hash table's entry count")?;
    let mut in_bucket: u16 = 0;
    // Each entry takes at least 26 bytes, so a count the bytes can't hold ends in `Truncated`.
    for _ in 0..entries {
        if in_bucket == 0 {
            in_bucket = cur.u16("a bucket's entry count")?;
            if in_bucket == 0 {
                return Err(ReadError::Malformed {
                    at: cur.at,
                    what: "an empty bucket in the payload",
                });
            }
        }
        in_bucket -= 1;
        let _key_hash = cur.u64("an entry's key hash")?;
        let key_len = cur.u64("an entry's key length")?;
        let data_len = cur.u64("an entry's data length")?;
        let name = cur.take(key_len, "an entry's name")?;
        let start = cur.at;
        let data = cur.take(data_len, "an entry's records")?;
        records(Cursor { bytes: data, at: 0 }, start, version, name, &mut each)?;
    }
    Ok(Header { version: u32::try_from(version).unwrap_or(u32::MAX), flags })
}

/// The records of one name (`InstrProfLookupTrait::ReadData`), `start` where its data is in
/// the file, for errors.
fn records<'a>(
    mut data: Cursor<'a>,
    start: usize,
    version: u64,
    name: &'a [u8],
    each: &mut impl FnMut(Record<'a>),
) -> Result<(), ReadError> {
    let at = |data: &Cursor<'_>| start + data.at;
    while data.at < data.bytes.len() {
        let _hash = data
            .u64("a record's hash")
            .map_err(|_| ReadError::Truncated { at: at(&data), what: "a record's hash" })?;
        let counts = data.u64("a record's counter count")?;
        let counters = data
            .take(counts.saturating_mul(8), "a record's counters")
            .map_err(|_| ReadError::Truncated { at: at(&data), what: "a record's counters" })?;
        if version > 10 {
            let bitmap = data.u64("a record's bitmap length")?;
            data.take(bitmap.saturating_mul(8), "a record's bitmap")
                .map_err(|_| ReadError::Truncated { at: at(&data), what: "a record's bitmap" })?;
        }
        // `ValueProfData`: its total size, a `u32` it counts itself in, then the rest.
        let total = data.u32("a record's value profile size")?;
        if total < 8 {
            return Err(ReadError::Malformed {
                at: at(&data),
                what: "a value profile smaller than its own header",
            });
        }
        data.take(u64::from(total) - 4, "a record's value profile").map_err(|_| {
            ReadError::Truncated { at: at(&data), what: "a record's value profile" }
        })?;
        each(Record { name, counters });
    }
    Ok(())
}

/// The indexed profile, format 12, of `records` (name, hash, counters), as `llvm-profdata
/// merge` lays it out, with `flags` in its version: what the tests' profiles are. One bucket
/// per name; the bucket array is written with no entries in it, which this reader never reads.
#[cfg(test)]
pub(crate) fn written(flags: u64, records: &[(&str, u64, &[u64])]) -> Vec<u8> {
    let mut out = Vec::new();
    let word = |out: &mut Vec<u8>, it: u64| out.extend_from_slice(&it.to_le_bytes());
    word(&mut out, MAGIC);
    word(&mut out, 12 | flags);
    word(&mut out, 0);
    word(&mut out, 0);
    let hash_offset_at = out.len();
    word(&mut out, 0);
    for _ in 0..4 {
        word(&mut out, 0);
    }
    // A summary of no fields and no cutoffs.
    word(&mut out, 0);
    word(&mut out, 0);
    for &(name, hash, counts) in records {
        out.extend_from_slice(&1u16.to_le_bytes());
        word(&mut out, 0);
        word(&mut out, name.len() as u64);
        let data_len = 8 + 8 + 8 * counts.len() + 8 + 8;
        word(&mut out, data_len as u64);
        out.extend_from_slice(name.as_bytes());
        word(&mut out, hash);
        word(&mut out, counts.len() as u64);
        for &it in counts {
            word(&mut out, it);
        }
        word(&mut out, 0);
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let hash_offset = out.len() as u64;
    out[hash_offset_at..hash_offset_at + 8].copy_from_slice(&hash_offset.to_le_bytes());
    word(&mut out, 1);
    word(&mut out, records.len() as u64);
    word(&mut out, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_back_with_their_counters() {
        let bytes = written(IR_PROF | INSTR_ENTRY, &[("a", 7, &[3, 4]), ("b", 9, &[])]);
        let mut seen = Vec::new();
        let header =
            read(&bytes, |it| seen.push((it.name.to_vec(), it.counts().collect::<Vec<_>>())))
                .unwrap();
        assert_eq!(header.version, 12);
        assert!(header.entry_counted());
        assert_eq!(seen, [(b"a".to_vec(), vec![3, 4]), (b"b".to_vec(), vec![])]);
    }

    #[test]
    fn what_is_not_an_indexed_profile_is_an_error_naming_where() {
        assert_eq!(read(b"not a profile", |_| {}).err(), Some(ReadError::NotIndexed));
        let mut bytes = written(IR_PROF, &[("a", 7, &[3, 4])]);
        bytes[8] = 99;
        assert!(matches!(read(&bytes, |_| {}), Err(ReadError::Version(_))));
        let bytes = written(IR_PROF, &[("a", 7, &[3, 4])]);
        let cut = &bytes[..bytes.len() / 2];
        assert!(
            matches!(read(cut, |_| {}), Err(ReadError::Truncated { .. })),
            "{:?}",
            read(cut, |_| {}).err()
        );
        assert!(matches!(read(&written(BYTE_COVERAGE, &[]), |_| {}), Err(ReadError::Kind(_))));
    }
}
