//! Reads the wasm binary format: numbers, names, and the sections of a module or an object.

use std::ops::Range;

pub(super) use blaze_format::cursor::Reader;

/// What wasm reads beyond the cursor's fields. Its numbers are LEB128 (`uleb32`, `uleb`,
/// `sleb32`, `sleb`), never the cursor's fixed-width `u32`/`u64`/`i32`/`i64`.
pub(super) trait Wasm<'a> {
    /// A count of things that follow, each at least a byte: more than the bytes left is
    /// malformed, and is not allocated for.
    fn count(&mut self) -> Result<usize, String>;
    /// A name: its length, then that many bytes of UTF-8.
    fn name(&mut self) -> Result<&'a str, String>;
    /// `i32.const value; end`, the offset of an active segment of wasm32.
    fn i32_const(&mut self) -> Result<i32, String>;
}

impl<'a> Wasm<'a> for Reader<'a> {
    fn count(&mut self) -> Result<usize, String> {
        let count = self.uleb32()? as usize;
        if count > self.left() {
            // alloc: error path
            return Err(format!("a count of {count} with {} bytes left", self.left()));
        }
        Ok(count)
    }

    fn name(&mut self) -> Result<&'a str, String> {
        let len = self.uleb32()? as usize;
        let bytes = self.take(len)?;
        // alloc: error path
        std::str::from_utf8(bytes).map_err(|_| "a name that is not UTF-8".to_owned())
    }

    fn i32_const(&mut self) -> Result<i32, String> {
        match self.u8()? {
            0x41 => {}
            // alloc: error path
            other => return Err(format!("an offset expression of opcode {other:#04x}")),
        }
        let value = self.sleb32()?;
        match self.u8()? {
            0x0b => Ok(value),
            // alloc: error path
            _ => Err("an offset expression longer than a constant".to_owned()),
        }
    }
}

/// A section of a module or an object.
pub(super) struct Section<'a> {
    pub id: u8,
    /// What follows its id and size, and where that is in the file.
    pub contents: &'a [u8],
    pub at: usize,
}

impl Section<'_> {
    pub(super) fn range(&self) -> Range<usize> {
        self.at..self.at + self.contents.len()
    }
}

pub(super) const MAGIC: &[u8; 8] = b"\0asm\x01\0\0\0";

/// The sections of a wasm file, in order: one exact allocation, the headers walked twice
/// (the first walk counts and checks them, a few bytes per section).
pub(super) fn sections(bytes: &[u8]) -> Result<Vec<Section<'_>>, String> {
    if !bytes.starts_with(MAGIC) {
        // alloc: error path
        return Err("not a wasm file of version 1".to_owned());
    }
    let mut count = 0;
    each_section(bytes, |_| count += 1)?;
    let mut sections = Vec::with_capacity(count);
    each_section(bytes, |section| sections.push(section))?;
    Ok(sections)
}

fn each_section<'a>(bytes: &'a [u8], mut f: impl FnMut(Section<'a>)) -> Result<(), String> {
    let mut reader = Reader::starting_at(bytes, MAGIC.len())?;
    while !reader.is_done() {
        let id = reader.u8()?;
        let len = reader.uleb32()? as usize;
        let at = reader.at();
        // alloc: error path
        let contents = reader.take(len).map_err(|_| format!("section {id} is truncated"))?;
        f(Section { id, contents, at });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let mut reader = Reader::new(&[0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x01]);
        assert_eq!(reader.uleb32().unwrap(), 624_485);
        assert_eq!(reader.sleb32().unwrap(), -1);
        assert_eq!(reader.uleb().unwrap(), 128);
        assert!(reader.is_done());
        assert!(reader.u8().is_err());
    }

    #[test]
    fn too_long_or_too_large() {
        assert!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).uleb32().is_err());
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x1f]).uleb32().is_err());
        assert_eq!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f]).uleb32().unwrap(), u32::MAX);
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x77]).sleb32().is_err());
        assert!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x08]).sleb32().is_err());
        assert_eq!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x78]).sleb32().unwrap(), i32::MIN);
        assert_eq!(Reader::new(&[0xc0, 0xbb, 0x78]).sleb().unwrap(), -123_456);
    }

    #[test]
    fn names_and_counts() {
        let mut reader = Reader::new(&[2, b'h', b'i', 9]);
        assert_eq!(reader.name().unwrap(), "hi");
        assert!(reader.count().is_err());
        assert!(Reader::new(&[1, 0xff]).name().is_err());
    }

    #[test]
    fn reads_allocate_nothing() {
        let numbers =
            [0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x01, 0xc0, 0xbb, 0x78, 0xff, 0xff, 0xff, 0xff, 0x0f];
        let named = [2, b'h', b'i', 0x41, 0x7f, 0x0b, 1, 0];
        let (numbers, rest) = blaze_alloc_count::assert_at_most(0, "LEB128 and name reads", || {
            let mut r = Reader::new(&numbers);
            let numbers = (r.uleb32(), r.sleb32(), r.uleb(), r.sleb(), r.uleb32());
            let mut r = Reader::new(&named);
            let name = r.name().map(str::len);
            (numbers, (name, r.i32_const(), r.count(), r.take(0).map(<[u8]>::len), r.is_done()))
        });
        assert_eq!(numbers, (Ok(624_485), Ok(-1), Ok(128), Ok(-123_456), Ok(u32::MAX)));
        assert_eq!(rest, (Ok(2), Ok(-1), Ok(1), Ok(0), false));
    }

    #[test]
    fn sections_in_order() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend([1, 2, 0xaa, 0xbb, 0, 1, 0]);
        let sections = sections(&bytes).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!((sections[0].id, sections[0].contents), (1, &[0xaa, 0xbb][..]));
        assert_eq!(sections[0].range(), 10..12);
        assert_eq!(sections[1].id, 0);
        assert!(super::sections(b"\0asm\x02\0\0\0").is_err());
        bytes.extend([1, 9]);
        assert!(super::sections(&bytes).is_err());
    }
}
