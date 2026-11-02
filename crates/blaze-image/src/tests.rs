use std::sync::{Arc, atomic::Ordering::Relaxed};

use blaze_format::schema;
use blaze_ids::rng::SplitMix64 as Rng;

use super::*;

const PAGE: u32 = 12;
const A: SectionId = SectionId::DB_MEMOS;
const B: SectionId = SectionId::SYMBOLS;
const C: SectionId = SectionId::PLANE_HISTORY;
const D: SectionId = SectionId::vfs(1);

fn schema_a() -> SchemaHash {
    schema("test section a 1")
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Sections of 3 groups and a bit, an empty one, one group exactly, and a few bytes.
fn contents() -> Vec<(SectionId, Vec<u8>)> {
    vec![
        (A, noise(3 * 4096 + 100, 1)),
        (B, Vec::new()),
        (C, noise(4096, 2)),
        (D, noise(37, 3)),
    ]
}

fn build(shift: u32, sections: &[(SectionId, Vec<u8>)]) -> Vec<u8> {
    let mut builder = Builder::with_group_shift(shift);
    for (id, bytes) in sections {
        builder.add(*id, schema_a(), bytes);
    }
    builder.finish().unwrap()
}

fn open(bytes: Vec<u8>) -> Image<Vec<u8>> {
    Image::open(bytes).unwrap()
}

#[test]
fn sections_read_back_whole_and_by_group() {
    let sections = contents();
    let image = open(build(PAGE, &sections));
    assert_eq!(image.sections().len(), sections.len());
    let mut rng = Rng::new(10);
    for (id, bytes) in &sections {
        let section = image.section(*id, schema_a()).unwrap();
        assert_eq!(section.len, bytes.len() as u64);
        assert_eq!(section.at % 4096, 0, "{id} starts on a group");
        assert_eq!(image.whole(&section).unwrap(), &bytes[..]);
        let mut joined = Vec::new();
        for group in 0..section.groups {
            joined.extend_from_slice(image.group(&section, group).unwrap());
        }
        assert_eq!(joined, *bytes);
        for _ in 0..50 {
            let at = rng.below(section.len + 1);
            let len = rng.below(section.len - at + 1);
            assert_eq!(image.range(&section, at, len).unwrap(), &bytes[at as usize..(at + len) as usize]);
        }
    }
    assert!(image.verify().is_empty());
}

#[test]
fn the_same_sections_give_the_same_bytes() {
    assert_eq!(build(PAGE, &contents()), build(PAGE, &contents()));
    let other = {
        let mut sections = contents();
        sections[3].1[0] ^= 1;
        build(PAGE, &sections)
    };
    let (one, two) = (open(build(PAGE, &contents())), open(other));
    assert_ne!(one.hash(), two.hash(), "a changed byte is a changed image hash");
}

/// Opening reads the spine and nothing past it: no section's byte, no CRC table byte.
#[test]
fn open_reads_only_the_spine() {
    for shift in [PAGE, DEFAULT_GROUP_SHIFT] {
        let sections = contents();
        let bytes = build(shift, &sections);
        let image = open(bytes);
        let spine_len = (HEADER + sections.len() * SECTION_ENTRY + 4).next_multiple_of(8) as u64;
        assert_eq!(image.touched.end.load(Relaxed), spine_len, "2^{shift}: read to the spine's end");
        assert_eq!(image.touched.bytes.load(Relaxed), HEADER as u64 + spine_len, "the header, then the spine");
        for section in image.sections() {
            for group in 0..section.groups {
                assert!(!image.is_checked(section, group));
            }
        }
    }
}

/// A read checks the groups it reaches, and a large image's first read pages in one group of
/// data and one of the CRC table.
#[test]
fn a_read_checks_only_its_groups() {
    let sections = vec![(A, noise(40 * 4096, 9))];
    let image = open(build(PAGE, &sections));
    let section = image.section(A, schema_a()).unwrap();
    let before = image.touched.bytes.load(Relaxed);
    assert_eq!(image.range(&section, 5 * 4096 + 10, 8).unwrap(), &sections[0].1[5 * 4096 + 10..][..8]);
    let read = image.touched.bytes.load(Relaxed) - before;
    assert_eq!(read, 4096 + 4096 + 4 + 8, "the group, the table's group, the entry and the bytes");
    for group in 0..section.groups {
        assert_eq!(image.is_checked(&section, group), group == 5, "group {group}");
    }
    image.range(&section, 7 * 4096 - 2, 4).unwrap();
    assert!(image.is_checked(&section, 6) && image.is_checked(&section, 7));
    let again = image.touched.bytes.load(Relaxed);
    image.range(&section, 5 * 4096, 4096).unwrap();
    assert_eq!(image.touched.bytes.load(Relaxed) - again, 4096, "a checked group is read, not checked again");
}

/// Every group of every section, corrupted alone, fails on its first read naming its section
/// and group, and fails every read after; every other group still reads.
#[test]
fn every_corrupted_group_is_found_and_named() {
    let sections = contents();
    let good = build(PAGE, &sections);
    let layout = open(good.clone());
    let mut rng = Rng::new(11);
    for target in layout.sections() {
        for group in 0..target.groups {
            let mut bytes = good.clone();
            let in_group = rng.below(4096);
            bytes[(target.at + u64::from(group) * 4096 + in_group) as usize] ^= 1 << rng.below(8);
            let image = open(bytes);
            for section in image.sections() {
                for at in 0..section.groups {
                    let read = image.group(section, at);
                    if section.id == target.id && at == group {
                        let error = read.unwrap_err();
                        assert_eq!((error.section, error.group), (target.id, group));
                        assert!(matches!(error.cause, Cause::Crc { .. }), "{error}");
                        assert_eq!(image.group(section, at).unwrap_err(), error, "fails again");
                        assert!(image.is_bad(section, at));
                        let text = error.to_string();
                        assert!(text.contains(target.id.name()) && text.contains(&format!("group {group}")), "{text}");
                    } else {
                        read.unwrap();
                    }
                }
            }
            assert_eq!(image.verify().len(), 1);
        }
    }
}

/// A byte of the padding past a section's end is in its group's CRC too.
#[test]
fn padding_is_checked() {
    let sections = contents();
    let mut bytes = build(PAGE, &sections);
    let section = open(bytes.clone()).section(D, schema_a()).unwrap();
    bytes[(section.at + 4000) as usize] = 1;
    let image = open(bytes);
    let error = image.whole(&section).unwrap_err();
    assert_eq!((error.section, error.group), (D, 0));
}

/// A changed CRC table group makes every group whose CRC it holds uncheckable, by name, and
/// leaves the groups of other table groups readable.
#[test]
fn a_corrupt_crc_table_group_is_named() {
    // 1100 data groups: the table's first group holds 1024 CRCs, its second the rest.
    let sections = vec![(A, noise(1100 * 4096, 12))];
    let good = build(PAGE, &sections);
    let layout = open(good.clone());
    let section = layout.section(A, schema_a()).unwrap();
    let crc_at = good.len() - 2 * 4096;
    let mut bytes = good;
    bytes[crc_at + 4096 + 17] ^= 0x40;
    let image = open(bytes);
    for group in [0, 500, 1023] {
        image.group(&section, group).unwrap();
    }
    for group in [1024, 1050, 1099] {
        let error = image.group(&section, group).unwrap_err();
        assert_eq!(error.group, group);
        assert!(matches!(error.cause, Cause::CrcTable { table_group: 1, .. }), "{error}");
    }
}

/// A changed byte anywhere in the spine is refused at open.
#[test]
fn a_changed_spine_is_refused() {
    let good = build(PAGE, &contents());
    let spine_len = (HEADER + 4 * SECTION_ENTRY + 4).next_multiple_of(8);
    for at in 0..spine_len {
        let mut bytes = good.clone();
        bytes[at] ^= 0x10;
        assert!(Image::open(bytes).is_err(), "byte {at} of the spine changed");
    }
    let mut bytes = good.clone();
    bytes[spine_len + 3] ^= 1;
    Image::open(bytes).expect("the rest of group 0 is not the spine");
}

/// A torn write: the new image cut at every page boundary (what a write without the rename
/// leaves) is refused at open, and the old image, which the rename leaves whole, still opens.
#[test]
fn a_torn_write_is_the_old_image_or_refused() {
    let old = build(PAGE, &contents());
    let new = {
        let mut sections = contents();
        sections[0].1.extend_from_slice(&noise(5 * 4096, 13));
        build(PAGE, &sections)
    };
    for cut in (0..new.len()).step_by(4096).chain([1, HEADER - 1, HEADER, HEADER + 1, new.len() - 1]) {
        let error = Image::open(new[..cut].to_vec()).err();
        assert!(error.is_some(), "the new image cut at {cut} of {} opened", new.len());
    }
    assert!(open(old.clone()).verify().is_empty());
    assert!(open(new.clone()).verify().is_empty());
}

/// A write torn the other way, over the old file in place: the new image's first pages over the
/// old one's. Each read gives the bytes of the image whose spine survived, or a `Corrupt`:
/// never one image's bytes read as the other's.
#[test]
fn an_image_written_over_in_place_never_reads_old_bytes() {
    let sections = |seed| vec![(A, noise(9 * 4096, seed)), (C, noise(2 * 4096 + 5, seed + 1))];
    let old = build(PAGE, &sections(20));
    let new = build(PAGE, &sections(30));
    assert_eq!(old.len(), new.len());
    let old_hash = open(old.clone()).hash();
    let mut corrupt = 0;
    for cut in (0..=new.len()).step_by(4096) {
        let mut bytes = old.clone();
        bytes[..cut].copy_from_slice(&new[..cut]);
        let Ok(image) = Image::open(bytes) else { continue };
        let wanted = sections(if image.hash() == old_hash { 20 } else { 30 });
        for (id, contents) in &wanted {
            let section = image.section(*id, schema_a()).unwrap();
            for group in 0..section.groups {
                match image.group(&section, group) {
                    Ok(read) => {
                        let at = group as usize * 4096;
                        assert_eq!(read, &contents[at..at + read.len()], "cut {cut}: {id} group {group}");
                    }
                    Err(error) => {
                        assert_eq!((error.section, error.group), (*id, group));
                        corrupt += 1;
                    }
                }
            }
        }
    }
    assert!(corrupt > 0, "some cut left the new spine over old groups");
}

#[test]
fn sections_are_refused_by_schema_or_absence() {
    let image = open(build(PAGE, &contents()));
    assert_eq!(image.section(SectionId::DB_EDGES, schema_a()), Err(Refused::Missing { id: SectionId::DB_EDGES }));
    let other = schema("test section a 2");
    assert_eq!(image.section(A, other), Err(Refused::Schema { id: A, found: schema_a(), wanted: other }));
}

#[test]
fn reads_past_a_section_are_bounds_errors() {
    let image = open(build(PAGE, &contents()));
    let section = image.section(D, schema_a()).unwrap();
    assert_eq!(image.range(&section, 30, 7).unwrap().len(), 7);
    let error = image.range(&section, 30, 8).unwrap_err();
    assert_eq!(error.cause, Cause::Bounds { at: 30, len: 8 });
    assert!(image.range(&section, u64::MAX, 2).is_err());
    assert!(image.group(&section, 1).is_err());
    assert_eq!(image.range(&section, 37, 0).unwrap(), &[] as &[u8]);
}

#[test]
fn typed_slices_read_in_place() {
    let words: Vec<u64> = (0..2000).map(|it| it * 3 + 1).collect();
    let mut builder = Builder::with_group_shift(PAGE);
    let (bytes_at, words_at) = {
        let mut section = builder.section(A, schema_a());
        let bytes_at = section.bytes(b"abc");
        let words_at = section.slice(&words);
        let root = section.value(&0u64);
        section.patch(root, &7u64.to_le_bytes());
        (bytes_at, words_at)
    };
    let image = open(builder.finish().unwrap());
    let section = image.section(A, schema_a()).unwrap();
    assert_eq!((bytes_at, words_at), (0, 8));
    assert_eq!(image.slice::<u64>(&section, words_at, 2000).unwrap(), &words[..]);
    assert_eq!(*image.value::<u64>(&section, words_at + 2000 * 8).unwrap(), 7);
    let error = image.slice::<u64>(&section, 1, 1).unwrap_err();
    assert_eq!(error.cause, Cause::Misaligned { at: 1, align: 8 });
    assert_eq!(image.slice::<u64>(&section, 0, 0).unwrap(), &[] as &[u64], "an empty column");
    assert_eq!(image.slice::<u64>(&section, 1, 0).unwrap(), &[] as &[u64], "anywhere");
}

/// Threads reading the same unchecked groups at once all get their bytes, each group is
/// checked, and a corrupt one fails for every thread.
#[test]
fn first_reads_race() {
    let sections = vec![(A, noise(64 * 4096, 15))];
    let mut bytes = build(PAGE, &sections);
    let at = open(bytes.clone()).section(A, schema_a()).unwrap().at as usize;
    bytes[at + 33 * 4096 + 5] ^= 2;
    let image = Arc::new(open(bytes));
    let section = image.section(A, schema_a()).unwrap();
    std::thread::scope(|scope| {
        for thread in 0..8u64 {
            let (image, expected) = (&image, &sections[0].1);
            scope.spawn(move || {
                for step in 0..64u32 {
                    let group = (step + thread as u32 * 8) % 64;
                    match image.group(&section, group) {
                        Ok(read) => assert_eq!(read, &expected[group as usize * 4096..][..4096]),
                        Err(error) => assert_eq!(error.group, 33),
                    }
                }
            });
        }
    });
    for group in 0..64 {
        assert_eq!(image.is_checked(&section, group), group != 33);
        assert_eq!(image.is_bad(&section, group), group == 33);
    }
}

#[test]
fn group_sizes_and_limits() {
    for shift in [MIN_GROUP_SHIFT, 14, DEFAULT_GROUP_SHIFT] {
        let image = open(build(shift, &contents()));
        assert_eq!(image.group_len(), 1 << shift);
        assert_eq!(image.len() % (1 << shift), 0);
        assert!(image.verify().is_empty());
    }
    let empty = open(Builder::with_group_shift(PAGE).finish().unwrap());
    assert_eq!(empty.len(), 4096, "an empty image is its spine");
    let mut many = Builder::with_group_shift(PAGE);
    for id in 0..100 {
        many.add(SectionId::interned(id), schema_a(), &[id as u8]);
    }
    let error = many.finish().unwrap_err();
    assert_eq!(error.group, 4096);
}

#[test]
fn ids_name_their_sections() {
    assert_eq!(SectionId::vfs(3).name(), "the VFS tree");
    assert_eq!(SectionId::interned(9).name(), "an interned table");
    assert_eq!(SectionId(7).name(), "a section this blaze does not know");
    assert!(SectionId::SYMBOLS.to_string().contains("0x200"));
}

/// What the VFS keeps: a section as the shared mapping and a range of it, checked whole, the
/// same bytes `whole` gives; a corrupt group anywhere in it refuses the range.
#[test]
fn a_whole_section_is_a_checked_range_of_the_shared_bytes() {
    use blaze_core::Mapping;
    let contents = contents();
    let file = build(12, &contents);
    let mapping = Mapping::copy(&file);
    let image = Arc::new(Image::open(mapping.clone()).unwrap());
    assert!(image.owner().same(&mapping));
    for (id, bytes) in &contents {
        let section = image.section(*id, schema_a()).unwrap();
        let range = image.whole_range(&section).unwrap();
        let kept: (Mapping, std::ops::Range<usize>) = (image.owner().clone(), range.clone());
        assert_eq!(&kept.0.bytes()[kept.1], &bytes[..]);
        assert_eq!(range.start as u64, section.at);
        assert!((0..section.groups).all(|group| image.is_checked(&section, group)));
    }

    let mut corrupt = file;
    let section = open(corrupt.clone()).section(A, schema_a()).unwrap();
    corrupt[section.at as usize + 2 * 4096 + 5] ^= 1;
    let image = Image::open(Mapping::copy(&corrupt)).unwrap();
    let failed = image.whole_range(&image.section(A, schema_a()).unwrap()).unwrap_err();
    assert_eq!((failed.section, failed.group), (A, 2));
}

/// A section as `Bytes`: the checked bytes `whole` gives, 8-aligned, keeping the image alive
/// after every other handle is gone; a corrupt group refuses them.
#[test]
fn whole_bytes_keep_the_image_and_are_the_section() {
    use blaze_core::Mapping;
    let contents = contents();
    let file = build(12, &contents);
    let image = Arc::new(Image::open(Mapping::copy(&file)).unwrap());
    let kept: Vec<blaze_bytes::Bytes> = contents
        .iter()
        .map(|(id, _)| image.whole_bytes(&image.section(*id, schema_a()).unwrap()).unwrap())
        .collect();
    drop(image);
    for ((_, bytes), kept) in contents.iter().zip(&kept) {
        assert_eq!(&kept[..], &bytes[..]);
        assert!(kept.is_empty() || kept.as_ptr().addr().is_multiple_of(8));
    }

    let mut corrupt = file;
    let section = open(corrupt.clone()).section(A, schema_a()).unwrap();
    corrupt[section.at as usize + 4096 + 1] ^= 1;
    let image = Arc::new(Image::open(Mapping::copy(&corrupt)).unwrap());
    let failed = image.whole_bytes(&image.section(A, schema_a()).unwrap()).unwrap_err();
    assert_eq!((failed.section, failed.group), (A, 1));
}

/// CRCs computed on many workers give the image one core gives, byte for byte.
#[test]
fn an_image_finished_on_workers_is_the_serial_one() {
    let four = |n: usize, job: &(dyn Fn(usize) + Sync)| {
        std::thread::scope(|scope| {
            for worker in 0..4 {
                scope.spawn(move || (worker..n).step_by(4).rev().for_each(job));
            }
        });
    };
    let sections = [(A, noise(70 * 4096 + 9, 7)), (C, noise(33 * 4096, 8)), (D, noise(5, 9))];
    let mut serial = Builder::with_group_shift(12);
    let mut parallel = Builder::with_group_shift(12);
    for (id, bytes) in &sections {
        serial.add(*id, schema_a(), bytes);
        parallel.add(*id, schema_a(), bytes);
    }
    let parallel = parallel.finish_on(&four).unwrap();
    assert_eq!(parallel, serial.finish().unwrap());
    assert!(open(parallel).verify().is_empty());
}

/// A writer names the image it finished as a reader of it will: `hash_of` of the bytes is the
/// opened image's `hash`. Bytes that are not an image's have none.
#[test]
fn a_finished_image_is_named_by_its_hash() {
    let bytes = build(12, &contents());
    let named = hash_of(&bytes).unwrap();
    assert_eq!(named, open(bytes.clone()).hash());
    assert_ne!(Some(named), hash_of(&build(12, &contents()[..2])));
    assert_eq!(hash_of(&bytes[..HEADER - 1]), None);
    let mut other = bytes;
    other[0] ^= 1;
    assert_eq!(hash_of(&other), None);
}

/// Reads allocate nothing: finding a section, a group's first read (its CRC) and every read
/// after, typed or not.
#[test]
fn reads_allocate_nothing() {
    let sections = contents();
    let bytes = build(PAGE, &sections);
    blaze_alloc_count::assert_at_most(0, "naming an image's bytes", || hash_of(&bytes).unwrap());
    let image = open(bytes);
    for (id, want) in &sections {
        let section = blaze_alloc_count::assert_at_most(0, "finding a section", || {
            image.section(*id, schema_a()).unwrap()
        });
        let len = section.len;
        blaze_alloc_count::assert_at_most(0, "a first read", || {
            assert_eq!(image.range(&section, 0, len).unwrap(), &want[..]);
        });
        blaze_alloc_count::assert_at_most(0, "a read of checked groups", || {
            assert_eq!(image.whole(&section).unwrap(), &want[..]);
            assert_eq!(image.slice::<u64>(&section, 0, len / 8).unwrap().len() as u64, len / 8);
            image.check(&section, 0..section.groups).unwrap();
        });
    }
}
