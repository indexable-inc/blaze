//! The cost-explicit forms (D120): one test a module of forms ([`crate::vec`], [`crate::boxed`],
//! [`crate::rows`], [`crate::text`], [`crate::soa`], [`crate::map`], [`crate::set`],
//! [`crate::id`], [`crate::bits`], [`crate::paths`], [`crate::mem`]'s handle). Each builds its
//! forms by the bstd path its module's table names, in a task's frame from a [`Cx`] or in an
//! [`Alloc`] over that frame, and runs one operation, so the paths the docs name are the paths
//! that compile. These are not property tests: the forms' behaviour is tested in blaze-own and
//! blaze-kit; a wrong export (another type under the name, or none) fails here to build or to
//! give the value asserted. Each passes with the harness's `ok` line and prints nothing else.

use super::{root, world};
use crate::{
    Plant,
    cx::Cx,
    io::Io,
    mem::{Alloc, Erased, Frame},
};

crate::id::id! {
    /// A row of a test's table.
    struct Row;
}

crate::soa::soa! {
    struct Point, Points, PointRef {
        x: u32 => xs / xs_mut,
        y: u32 => ys / ys_mut,
    }
}

/// A simulated world and its root context, whose frame the forms are made in.
fn task() -> (Io, Cx) {
    let io = world(1, Plant::None);
    let cx = root(&io);
    (io, cx)
}

/// `n` squares, in the allocator the caller hands in: the shape of a function that allocates
/// (`alloc: Alloc<'_>`, compiled once for every allocator).
fn squares(n: u32, alloc: Alloc<'_>) -> crate::vec::Vec<u32, Alloc<'_>> {
    let mut out = crate::vec::Vec::new_in(alloc);
    for i in 0..n {
        out.push(i * i);
    }
    out
}

#[test]
fn the_vectors_are_made_in_a_frame() {
    use crate::vec::{FixedVec, InlineVec, Packed, Segs, SharedSegs, Vec, vec_in};
    let (_io, cx) = task();

    let mut grown = Vec::new_in(cx.frame());
    grown.push(7u32);
    let listed = vec_in![cx.frame(); 1u32, 2, 3];
    assert_eq!((grown.as_slice(), listed.as_slice()), (&[7][..], &[1, 2, 3][..]));

    let mut fixed: FixedVec<u32, 2> = FixedVec::new();
    assert!(fixed.push(1).is_ok() && fixed.push(2).is_ok());
    assert!(fixed.push(3).is_err(), "a FixedVec of 2 took a third value");

    let mut inline: InlineVec<u32, 4, &Frame> = InlineVec::new_in(cx.frame());
    inline.push(5);
    assert_eq!(inline.len(), 1);

    let mut segs = Segs::new_in(cx.frame());
    let at = segs.push(9u64);
    assert_eq!(segs.get(at), Some(&9));

    let shared: SharedSegs<u64, &Frame> = SharedSegs::new_in(cx.frame());
    {
        let mut appender = shared.appender().expect("the one appender of a new SharedSegs");
        appender.push(11);
    }
    assert_eq!(shared.get(0), Some(&11));

    let mut packed: Packed<u32, &Frame> = Packed::new_in(cx.frame());
    packed.push(&42);
    assert_eq!(packed.iter().next(), Some(42));
}

#[test]
fn a_box_rc_and_arc_are_made_in_a_named_allocator() {
    use crate::boxed::{Arc, Box, Rc};
    let (_io, cx) = task();
    let erased = Erased::new(cx.frame());
    let alloc = erased.handle();

    let boxed = Box::new_in(3u32, cx.frame());
    let rc = Rc::new_in(4u32, alloc);
    let arc = Arc::new_in(5u32, alloc);
    let again = Arc::clone(&arc);
    assert_eq!((*boxed, *Rc::clone(&rc), *again), (3, 4, 5));
}

#[test]
fn rows_are_two_arrays() {
    use crate::rows::Lists;
    let (_io, cx) = task();
    let mut rows = Lists::new_in(cx.frame());
    rows.push_row([1u32, 2]);
    rows.push_row([3u32]);
    assert_eq!((rows.len(), rows.row(0), rows.row(1)), (2, &[1, 2][..], &[3][..]));
}

#[test]
fn many_strings_are_one_buffer() {
    use crate::text::{FixedString, SortedTexts, Texts};
    let (_io, cx) = task();
    let mut texts = Texts::new_in(cx.frame());
    for name in ["serde", "clap", "serde"] {
        texts.push(name);
    }
    let sorted = SortedTexts::from_texts_in(&texts, cx.frame());
    assert_eq!((texts.len(), sorted.len(), sorted.contains("clap")), (3, 2, true));

    let mut short: FixedString<8> = FixedString::new();
    assert!(short.try_push_str("blaze").is_ok());
    assert!(short.try_push_str("-std!").is_err(), "a FixedString of 8 took 10 bytes");
}

#[test]
fn a_soa_keeps_a_column_a_field() {
    use crate::soa::Soa;
    let (_io, cx) = task();
    let mut pairs: Soa<(u32, u64), &Frame> = Soa::new_in(cx.frame());
    pairs.push((1, 2));
    let mut points = Points::new_in(cx.frame());
    points.push(Point { x: 3, y: 4 });
    assert_eq!((pairs.len(), points.len()), (1, 1));
}

#[test]
fn the_ordered_maps_say_which_they_are() {
    use crate::map::{BTreeMap, FlatMap};
    use crate::set::{BTreeSet, FlatSet};
    use crate::vec::vec_in;
    let (_io, cx) = task();

    let flat = FlatMap::from_sorted(vec_in![cx.frame(); (1u32, "one"), (2, "two")]).expect("increasing keys");
    let mut tree = BTreeMap::new_in(cx.frame());
    tree.insert(3u32, "three");
    assert_eq!((flat.get(&2), tree.get(&3)), (Some(&"two"), Some(&"three")));

    let flat_set = FlatSet::from_sorted(vec_in![cx.frame(); 1u32, 3, 5]).expect("increasing values");
    let mut tree_set = BTreeSet::new_in(cx.frame());
    tree_set.insert(8u32);
    assert!(flat_set.contains(&3) && tree_set.contains(&8));
}

#[test]
fn a_typed_id_indexes_its_own_tables() {
    use crate::id::{Table, TableMap, TableSet};
    let (_io, cx) = task();
    let mut table: Table<Row, &str, &Frame> = Table::new_in(cx.frame());
    let first = table.push("first");
    let mut marked: TableSet<Row, &Frame> = TableSet::new_in(cx.frame());
    marked.insert(first);
    let mut sizes: TableMap<Row, u32, &Frame> = TableMap::new_in(cx.frame());
    sizes.insert(first, 5);
    assert_eq!((table.get(first), marked.contains(first), sizes.get(first)), (Some(&"first"), true, Some(&5)));
}

#[test]
fn the_bit_set_and_the_sparse_set() {
    use crate::bits::{ChunkedBitSet, SparseSet};
    let (_io, cx) = task();
    let erased = Erased::new(cx.frame());
    let mut chunked: ChunkedBitSet<u32, Alloc<'_>> = ChunkedBitSet::new_empty_in(4096, erased.handle());
    chunked.insert(3000);
    let mut sparse: SparseSet<u32, &Frame> = SparseSet::new_in(64, cx.frame());
    sparse.insert(5);
    assert!(chunked.contains(3000) && !chunked.contains(2999) && sparse.contains(5));
}

#[test]
fn paths_are_rows_of_names() {
    use crate::paths::{DirId, Paths};
    let (_io, cx) = task();
    let mut paths = Paths::new_in(cx.frame());
    let src = paths.intern_dir(DirId::ROOT, "src");
    let lib = paths.intern(src, "lib.rs");
    assert_eq!((paths.name(lib), paths.find(src, "lib.rs")), ("lib.rs", Some(lib)));
}

/// An error whose words are 30 pieces of seven three-byte characters: 630 bytes.
#[derive(Debug)]
struct Long;

impl core::fmt::Display for Long {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for _ in 0..30 {
            f.write_str("€€€€€€€")?;
        }
        Ok(())
    }
}

impl core::error::Error for Long {}

/// The prelude's `Error` keeps its words inline (D120: `?` reaches no heap): a short message
/// whole, a long one cut at a character's boundary inside a piece (510 of 512 bytes; a cut by
/// whole pieces keeps 504) with the cut said. Passes with the harness's `ok` line.
#[test]
fn the_entrys_error_keeps_its_words_inline_and_says_when_cut() {
    use core::fmt::Write as _;

    use crate::{
        entry::{ERROR_BYTES, Error},
        text::FixedString,
    };
    let mut shown = FixedString::<1024>::new();
    write!(shown, "{}", Error::new("no such file: data/a.txt")).expect("1024 bytes hold it");
    assert_eq!(shown.as_str(), "no such file: data/a.txt");
    shown.clear();
    write!(shown, "{}", Error::from(Long)).expect("1024 bytes hold it");
    let text = shown.as_str();
    let words = text.split(" [cut").next().unwrap_or("");
    assert_eq!((ERROR_BYTES, words.len()), (512, 510));
    assert!(words.chars().all(|c| c == '€'));
    assert_eq!(&text[words.len()..], " [cut: the error said 630 bytes, and an `Error` keeps 512]");
}

#[test]
fn a_function_takes_the_allocator_as_one_word() {
    let (_io, cx) = task();
    let erased = Erased::new(cx.frame());
    let made = squares(4, erased.handle());
    assert_eq!(made.as_slice(), &[0, 1, 4, 9][..]);
    assert_eq!(size_of::<Alloc<'_>>(), size_of::<usize>());
}

#[test]
fn batteries_are_exposed_and_work_in_a_frame() {
    let (_io, cx) = task();
    let alloc = cx.frame();

    // bstd::diff
    let patch = crate::diff::create_patch(
        "a\nb\n",
        "a\nc\n",
        crate::diff::PatchOpts {
            algorithm: crate::diff::Algorithm::Myers,
            context: 1,
            old_name: None,
            new_name: None,
            budget: crate::diff::DEFAULT_BUDGET,
            alloc,
        },
    )
    .expect("patch");
    let applied = crate::diff::apply_patch_text(
        "a\nb\n",
        patch.as_str(),
        crate::diff::ParsePatchLimits::USUAL,
        crate::diff::ApplyOpts { max_offset: 0, alloc },
    )
    .expect("apply");
    assert_eq!(applied.as_str(), "a\nc\n");

    // bstd::hex + bstd::digest (HMAC-SHA256 + CRC32)
    let mac = crate::digest::hmac_sha256(b"key", b"msg");
    let mac_hex = crate::hex::encode_in(&mac, alloc).expect("hex");
    assert_eq!(crate::hex::decode_array::<32>(mac_hex.as_bytes()).expect("dec"), mac);
    let mut crc = crate::digest::Crc32::new();
    crc.update(b"123456789");
    assert_eq!(crc.finish(), 0xcbf4_3926);

    // bstd::ansi
    let painted = blaze_own::format_in!(
        alloc,
        "{}",
        crate::ansi::Style::new().fg(crate::ansi::Color::Green).paint("ok")
    );
    assert_eq!(crate::ansi::strip_in(painted.as_str(), alloc).expect("strip").as_str(), "ok");
    assert_eq!(crate::ansi::visible_width(painted.as_str()), 2);

    // bstd::uuid
    let u = crate::uuid::Uuid::v7(1_700_000_000_000, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(u.timestamp_ms(), Some(1_700_000_000_000));

    // bstd::csv
    let tbl = crate::csv::parse_in(
        "k,v\nx,1\n",
        crate::csv::CsvOpts {
            delimiter: b',',
            shape: crate::csv::RowShape::Rectangular,
            limits: crate::csv::CsvLimits::USUAL,
            alloc,
        },
    )
    .expect("csv");
    assert_eq!(tbl.row_count(), 2);

    // Facade re-exports: base64, json, cbor, toml, semver, compress, search, bitflags, tar
    let b64 = crate::base64::STANDARD.encode_in(b"blaze", alloc);
    assert_eq!(b64.as_str(), "YmxhemU=");

    let val = crate::json::read_in(b"{\"ok\":true}", crate::json::Limits::SHIPPED, alloc).expect("json");
    let cbor_bytes = crate::cbor::to_vec_in(&val, alloc);
    assert!(!cbor_bytes.is_empty());

    let toml_doc =
        crate::toml::Document::parse_in("name = \"blaze\"\n", crate::toml::Limits::SHIPPED, alloc)
            .expect("toml");
    assert_eq!(toml_doc.root().get("name").and_then(|v| v.as_str()), Some("blaze"));

    let ver = crate::semver::Version::parse_in("1.2.3", crate::semver::Limits::SHIPPED, alloc)
        .expect("semver");
    assert_eq!((ver.major, ver.minor, ver.patch), (1, 2, 3));

    let mut gz = crate::vec::Vec::new_in(alloc);
    crate::compress::gzip(b"hello blaze", crate::compress::Level::Fast, alloc, &mut gz)
        .expect("gzip");
    assert!(!gz.is_empty());

    assert_eq!(crate::search::memchr(b'z', b"blaze"), Some(3));
    let mut tar_buf = crate::vec::Vec::new_in(alloc);
    let mut writer = crate::tar::Writer::new();
    writer.finish(&mut tar_buf).expect("finish tar");
    assert!(!tar_buf.is_empty());
}
