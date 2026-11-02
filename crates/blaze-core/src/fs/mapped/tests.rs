use std::path::{Path, PathBuf};

use blaze_ids::rng::SplitMix64;
use hegel::TestCase;
use hegel::generators as gs;

use super::{Advice, MapOptions, Mapped};

fn dir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-core-mapped-{what}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The safe reference: the file read whole.
fn reference(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut bytes = vec![0; len];
    SplitMix64::new(seed).fill(&mut bytes);
    bytes
}

const ADVICE: [Advice; 4] = [Advice::Normal, Advice::Sequential, Advice::Random, Advice::WillNeed];

fn every_option() -> impl Iterator<Item = MapOptions> {
    [false, true].into_iter().flat_map(|populate| {
        [false, true].into_iter().flat_map(move |huge| {
            ADVICE.into_iter().map(move |advice| MapOptions::new().populate(populate).huge_pages(huge).advise(advice))
        })
    })
}

#[test]
fn a_mapping_has_the_bytes_a_read_gives_with_every_option() {
    let dir = dir("sizes");
    // Around a page, a 16 KiB page, and a 2 MiB huge page.
    for len in [1, 4095, 4096, 4097, 16 << 10, (16 << 10) + 1, 2 << 20, (2 << 20) + 3] {
        let path = dir.join(format!("{len}"));
        std::fs::write(&path, pattern(len, len as u64)).unwrap();
        let read = reference(&path);
        for options in every_option() {
            // SAFETY: the test's own file, written once above.
            let mapped = unsafe { options.open(&path) }.unwrap();
            assert_eq!(mapped.bytes(), &read[..], "{len} bytes, {options:?}");
            assert_eq!(mapped.len(), len);
        }
    }
}

#[test]
fn a_file_maps_through_its_handle_and_outlives_it() {
    let dir = dir("handle");
    let path = dir.join("pack");
    std::fs::write(&path, b"sealed pack").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    // SAFETY: the test's own file, never written again.
    let mapped = unsafe { Mapped::map(&file) }.unwrap();
    drop(file);
    assert_eq!(&*mapped, b"sealed pack");
    assert_eq!(mapped.as_ref(), b"sealed pack");
}

#[test]
fn an_empty_file_maps_nothing() {
    let dir = dir("empty");
    let path = dir.join("empty");
    std::fs::write(&path, b"").unwrap();
    // SAFETY: the test's own file.
    let mapped = unsafe { Mapped::open(&path) }.unwrap();
    assert!(mapped.is_empty());
    assert_eq!(mapped.bytes(), b"");
    assert!(Mapped::EMPTY.bytes().is_empty());
}

#[test]
fn a_missing_file_is_an_error() {
    let dir = dir("missing");
    // SAFETY: nothing is mapped.
    let failed = unsafe { Mapped::open(&dir.join("missing")) }.unwrap_err();
    assert_eq!(failed.kind(), std::io::ErrorKind::NotFound);
}

/// The store and the image replace a mapped file only by renaming another over it: the
/// mapping keeps the bytes it had.
#[cfg(unix)]
#[test]
fn a_mapping_keeps_its_bytes_when_another_file_is_renamed_over_it() {
    let dir = dir("renamed");
    let path = dir.join("image");
    std::fs::write(&path, b"first image").unwrap();
    // SAFETY: the file is replaced below only by a rename, never written in place.
    let mapped = unsafe { Mapped::open(&path) }.unwrap();
    let next = dir.join("image.next");
    std::fs::write(&next, b"second image, longer").unwrap();
    std::fs::rename(&next, &path).unwrap();
    assert_eq!(mapped.bytes(), b"first image");
    // SAFETY: as above.
    assert_eq!(unsafe { Mapped::open(&path) }.unwrap().bytes(), b"second image, longer");
}

/// The length is the one taken at map time: a file that grows after shows none of what was
/// added, and every read stays below that length.
#[test]
fn a_mapping_keeps_its_length_when_the_file_grows() {
    use std::io::Write as _;
    let dir = dir("grows");
    let path = dir.join("pack");
    std::fs::write(&path, pattern(5000, 11)).unwrap();
    // SAFETY: the file is only appended to below, past the mapped length, so no byte under the
    // slice changes; it is never written in place or cut.
    let mapped = unsafe { Mapped::open(&path) }.unwrap();
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&pattern(9000, 12)).unwrap();
    assert_eq!(mapped.len(), 5000);
    assert_eq!(mapped.bytes(), &pattern(5000, 11)[..]);
    assert!(mapped.get(5000).is_none());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 14_000);
}

#[test]
fn reading_a_mapping_allocates_nothing() {
    let dir = dir("alloc");
    let path = dir.join("file");
    std::fs::write(&path, pattern(64 << 10, 7)).unwrap();
    // SAFETY: the test's own file, written once above.
    let mapped = unsafe { Mapped::open(&path) }.unwrap();
    let sum = blaze_alloc_count::assert_at_most(0, "reading a mapping", || {
        let mut sum = 0u64;
        for round in 0..1000 {
            let bytes = mapped.bytes();
            sum = sum.wrapping_add(u64::from(bytes[(round * 61) % bytes.len()]));
            sum = sum.wrapping_add(bytes.get(100..200).map_or(0, |it| it.len() as u64));
        }
        sum
    });
    assert_ne!(sum, 0);
}

#[test]
fn a_mapping_is_shared_across_threads() {
    const fn shared<T: Send + Sync>() {}
    shared::<Mapped>();
    let dir = dir("threads");
    let path = dir.join("file");
    std::fs::write(&path, pattern(1 << 20, 3)).unwrap();
    let read = reference(&path);
    // SAFETY: the test's own file, written once above.
    let mapped = std::sync::Arc::new(unsafe { Mapped::open(&path) }.unwrap());
    std::thread::scope(|scope| {
        for n in 0..4 {
            let (mapped, read) = (&mapped, &read);
            scope.spawn(move || {
                let at = n * (1 << 18);
                assert_eq!(&mapped[at..at + (1 << 18)], &read[at..at + (1 << 18)]);
            });
        }
    });
}

/// The mapping against the safe reference, for any bytes and any options.
#[hegel::test(test_cases = 64)]
fn a_mapping_is_the_file_read(tc: TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(39_999));
    let populate = tc.draw(gs::booleans());
    let huge = tc.draw(gs::booleans());
    let advice: usize = tc.draw(gs::integers().max_value(3));
    let dir = std::env::temp_dir().join(format!("blaze-core-mapped-prop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{:016x}", name_of(&bytes)));
    std::fs::write(&path, &bytes).unwrap();
    let options = MapOptions::new().populate(populate).huge_pages(huge).advise(ADVICE[advice]);
    // SAFETY: the case's own file; cases run one at a time, and each drops its mapping
    // before the next writes a file.
    let mapped = unsafe { options.open(&path) }.unwrap();
    assert_eq!(mapped.bytes(), &reference(&path)[..]);
    assert_eq!(mapped.bytes(), &bytes[..]);
}

/// FNV-1a: a name for a case's file.
fn name_of(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}
