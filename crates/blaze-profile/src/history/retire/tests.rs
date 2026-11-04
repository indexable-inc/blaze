use std::{collections::BTreeSet, path::Path};

use super::*;

/// A reader racing a merge, in every order of its steps and the writer's: whatever the order,
/// every file the reader resolved is still on disk when it reads, and it reads each flush's
/// rows exactly once (never the merged file and its inputs both, never neither). Until
/// 2026-11-04 the writer deleted a merge's inputs right after renaming its output into place,
/// and `blaze slow` failed with DuckDB's "Cannot open file .../0-00002976.spans.parquet".
#[test]
fn a_reader_racing_a_merge_reads_each_row_once_and_no_file_goes_from_under_it() {
    let inputs: Vec<String> = (1..=3).map(|seq| format!("1-{seq:08}.spans.parquet")).collect();
    let merged = "1-00000001-00000003-1.spans.parquet".to_owned();
    let spans = schema::TABLES.iter().position(|it| it.name == "spans").unwrap();
    // The writer's 5 steps and the reader's 5, in every one of the C(10, 5) orders.
    let orders = (0u32..1 << 10).filter(|it| it.count_ones() == 5);
    let mut ran = 0;
    for order in orders {
        let mut disk: BTreeSet<String> = inputs.iter().cloned().collect();
        disk.insert("schema.spans.parquet".to_owned());
        let mut retired = Retired::default();
        let mut published = retired.clone();
        let (mut writer, mut reader) = (0, 0);
        // The writer's view of the pins (W3) and what it may delete (W4).
        let mut reapable = Vec::new();
        let mut scanned: Vec<Pinned> = Vec::new();
        // The reader's pin: made, and the generation it said; what it read and resolved.
        let mut pin: Option<Pinned> = None;
        let mut read = Retired::default();
        let mut snapshot = None;
        for step in 0..10 {
            if order & (1 << step) == 0 {
                match writer {
                    // The merge's output renamed into place.
                    0 => _ = disk.insert(merged.clone()),
                    // Its inputs retired, and that published.
                    1 => {
                        assert!(retired.retire(inputs.iter().cloned()));
                        published = retired.clone();
                    }
                    // The pins looked at.
                    2 => scanned = pin.into_iter().collect(),
                    // The files no pin holds deleted.
                    3 => {
                        reapable = retired.reapable(&scanned);
                        for &at in &reapable {
                            disk.remove(&retired.files[at].0);
                        }
                    }
                    // Then forgotten, and that published.
                    _ => {
                        retired.reaped(&reapable);
                        published = retired.clone();
                    }
                }
                writer += 1;
            } else {
                match reader {
                    0 => pin = Some(None),
                    1 => read = published.clone(),
                    2 => pin = Some(Some(read.generation)),
                    3 => {
                        let listing: Vec<String> = disk.iter().cloned().collect();
                        snapshot = Some(Snapshot::resolve(Path::new("/h"), &listing, &read));
                    }
                    _ => {
                        let snapshot = snapshot.as_ref().unwrap();
                        assert_eq!(snapshot.dir(), Path::new("/h"));
                        let files = snapshot.files(spans);
                        for file in files {
                            assert!(
                                disk.contains(file),
                                "order {order:010b}: {file} was deleted under the reader"
                            );
                        }
                        let mut rows: Vec<u64> = files
                            .iter()
                            .filter_map(|it| Name::parse(it))
                            .flat_map(|it| it.first..=it.last)
                            .collect();
                        rows.sort_unstable();
                        assert_eq!(
                            rows,
                            [1, 2, 3],
                            "order {order:010b}: the reader read {files:?}"
                        );
                        assert!(files.contains(&"schema.spans.parquet".to_owned()));
                    }
                }
                reader += 1;
            }
        }
        // The reader gone, the next look at the pins finds none: every input goes.
        let at = retired.reapable(&[]);
        for &it in &at {
            disk.remove(&retired.files[it].0);
        }
        retired.reaped(&at);
        assert!(retired.files.is_empty() && retired.generation == 1);
        assert_eq!(disk, BTreeSet::from([merged.clone(), "schema.spans.parquet".to_owned()]));
        ran += 1;
    }
    assert_eq!(ran, 252);
}

/// [`RETIRED`] reads back as written, byte for byte again, and a pin's files are told apart by
/// their names.
#[test]
fn the_retired_file_round_trips_and_pins_parse() {
    let mut retired = Retired::default();
    assert!(
        retired
            .retire(["1-00000001.spans.parquet".to_owned(), "1-00000002.spans.parquet".to_owned()])
    );
    assert!(!retired.retire(["1-00000001.spans.parquet".to_owned()]), "a file is retired once");
    assert!(retired.retire(["0-00000003.folded.parquet".to_owned()]));
    let text = retired.encode();
    let back = Retired::decode(&text).unwrap();
    assert_eq!(back, retired);
    assert_eq!(back.encode(), text);
    assert_eq!(Retired::decode(&Retired::default().encode()), Ok(Retired::default()));
    assert!(Retired::decode("generation 1\n").is_err());
    assert!(Retired::decode(&format!("{HEADER}\ngeneration 1\n2 a.spans.parquet\n")).is_err());

    assert_eq!(PinFile::parse("12-3.pin"), Some(PinFile::Lock { id: "12-3" }));
    assert_eq!(PinFile::parse("12-3.7.gen"), Some(PinFile::Said { id: "12-3", generation: 7 }));
    assert_eq!(PinFile::parse(".12-3.tmp"), Some(PinFile::Making { id: "12-3" }));
    assert_eq!(PinFile::parse("12.pin"), None);
    assert_eq!(PinFile::Said { id: "12-3", generation: 7 }.file_name(), "12-3.7.gen");
    assert_eq!(PinFile::Lock { id: "12-3" }.pid(), Some(12));
    assert_eq!(pinned(&["1-1", "2-1"], &[("2-1", 4)]), [None, Some(4)]);
}
