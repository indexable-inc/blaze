//! The hooks of the `intern_table` fuzz target (`fuzz/targets/intern_table.rs`), built with the
//! `fuzz` feature only. [`restore`] restores a table section from any bytes, in a fresh table
//! of a fixed small text shape. It does not use the process's tables, which restore once.
//! [`section`] writes a section of given values, as seeds.
//!
//! An unsafe module for one pair of blocks: [`restore`] frees the image it restored from once
//! the table is gone. `Table::restore` reads its image as `&'static`, as the daemon's restore
//! does once a process. The safe version leaks each input's image, which grows a fuzz run's
//! memory by every input it tries.

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_image::{Builder, Image, SectionId};

pub use crate::table::RestoreError;
use crate::{
    order_key,
    table::{Live, Shape, Table, serial},
};

const ID: SectionId = SectionId::SYMBOLS;
const SCHEMA: SchemaHash = SchemaHash(ContentId([0x1f; 32]));

/// Compiled in: "a" twice, so the first is the one interning gives.
const STATICS: [&str; 3] = ["a", "b", "a"];

fn shape() -> Shape {
    Shape {
        statics: STATICS.len() as u32,
        static_bytes: |id| STATICS[id as usize - 1].as_bytes(),
        key: order_key,
        align: 1,
        size: 1,
        text: true,
    }
}

fn image(section: &[u8]) -> Image<Vec<u8>> {
    let mut builder = Builder::new();
    builder.add(ID, SCHEMA, section);
    Image::open(builder.finish().expect("a fuzz input's image fits")).expect("an image just built opens")
}

/// The section `table` writes with the IDs `live` marks.
fn write(table: &Table, live: &Live) -> Vec<u8> {
    let mut builder = Builder::new();
    table.write(live, &mut builder.section(ID, SCHEMA));
    let image = Image::open(builder.finish().expect("a written table's image fits")).expect("a written image opens");
    let section = image.section(ID, SCHEMA).expect("a written section is there");
    image.whole(&section).expect("a written section reads whole").to_vec()
}

/// A section restored into a fresh table of the hook's text shape, read back.
#[derive(Debug)]
pub struct Restored {
    /// The first ID not compiled in: the shape's statics plus one.
    pub first: u32,
    /// The table's end once restored: one past the highest ID it has had.
    pub end: u32,
    /// Every ID in `1..end` the table gives bytes for, ascending, with its bytes (the
    /// compiled-in ones included).
    pub values: Vec<(u32, Box<[u8]>)>,
    /// The table's `find` of each of `values`' bytes, in the same order.
    pub found: Vec<Option<u32>>,
    /// The restored table written again with every ID of `values` live, as [`section`] returns
    /// a section's bytes.
    pub written: Vec<u8>,
}

fn read(table: &Table) -> Restored {
    let first = shape().statics + 1;
    let end = table.end();
    let values: Vec<(u32, Box<[u8]>)> = (1..end).filter_map(|id| Some((id, table.get(id)?.into()))).collect();
    let found = values.iter().map(|(_, bytes)| table.find(bytes)).collect();
    let mut live = Live::new();
    values.iter().filter(|&&(id, _)| id >= first).for_each(|&(id, _)| live.mark_id(id));
    Restored { first, end, values, found, written: write(table, &live) }
}

/// `read` of the table `section` restores, its image freed after. `read` returns what it copied
/// out of the table, never a borrow of it (both callers are here).
fn with_restored<T>(section: &[u8], read: impl FnOnce(&Table) -> T) -> Result<T, RestoreError> {
    let at: *mut Image<Vec<u8>> = Box::into_raw(Box::new(image(section)));
    let out = {
        // SAFETY: `at` is a live box, freed only below, once the table made from it and every
        // borrow of it are gone. `Table::restore` keeps its `&'static` in the table alone (no
        // static, no thread), the table drops at the end of this block, and `read` copies out
        // what it returns. A table or a value's bytes kept past this block would be read after
        // they were freed.
        let image: &'static Image<Vec<u8>> = unsafe { &*at };
        let restored = image.section(ID, SCHEMA).map_err(RestoreError::from);
        restored.and_then(|section| Table::restore(shape(), image, &section, &serial)).map(|table| read(&table))
    };
    // SAFETY: `at` came from `Box::into_raw` above and is freed once, here, after the last use
    // of every borrow of it (above).
    drop(unsafe { Box::from_raw(at) });
    out
}

/// The table `section` holds, restored into a fresh table and read back; `Err` names what is
/// wrong and where. Shares nothing between calls, so calls may run on several threads at once.
pub fn restore(section: &[u8]) -> Result<Restored, RestoreError> {
    with_restored(section, read)
}

/// A section of a table holding `values`, interned in order (the first new one is ID 4), with
/// every ID `freed` names left free: a seed.
pub fn section(values: &[&[u8]], freed: &[u32]) -> Vec<u8> {
    let table = Table::new(shape());
    for value in values {
        table.intern(value);
    }
    let mut live = Live::new();
    (shape().statics + 1..table.end()).filter(|id| !freed.contains(id)).for_each(|id| live.mark_id(id));
    write(&table, &live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_section_restores_its_values() {
        let values: [&[u8]; 4] = [b"x", b"yy", "é".as_bytes(), b""];
        let restored = restore(&section(&values, &[5])).unwrap();
        let ids: Vec<u32> = restored.values.iter().map(|&(id, _)| id).collect();
        assert_eq!((restored.first, restored.end), (4, 8));
        assert_eq!(ids, [1, 2, 3, 4, 6, 7]);
        assert_eq!(&*restored.values[4].1, "é".as_bytes());
        assert_eq!(restored.found, [Some(1), Some(2), Some(1), Some(4), Some(6), Some(7)]);
        let again = restore(&restored.written).unwrap();
        assert_eq!((again.end, &again.values), (restored.end, &restored.values));
        assert_eq!(again.written, restored.written);
        let empty = restore(&section(&[], &[])).unwrap();
        assert_eq!((empty.end, empty.values.len()), (4, 3));
    }

    #[test]
    fn bytes_that_are_no_section_are_refused() {
        assert!(restore(b"").is_err());
        assert!(restore(&[0xff; 100]).is_err());
    }
}
