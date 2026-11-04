use std::collections::BTreeSet;

use super::{super::compact::Name, *};

/// A reader racing a merge, in every order of its steps and the writer's: whatever the order,
/// every file the manifest it read lists is still on disk when it reads, and it reads each
/// flush's rows exactly once (never the merged file and its inputs both, never neither). Until
/// 2026-11-04 the writer deleted a merge's inputs right after renaming its output into place,
/// and `blaze slow` failed with DuckDB's "Cannot open file .../0-00002976.spans.parquet".
#[test]
fn a_reader_racing_a_merge_reads_each_row_once_and_no_file_goes_from_under_it() {
    let inputs: Vec<String> = (1..=3).map(|seq| format!("1-{seq:08}.spans.parquet")).collect();
    let merged = "1-00000001-00000003-1.spans.parquet".to_owned();
    // The writer's 5 steps and the reader's 4, in every one of the C(9, 4) orders.
    let orders = (0u32..1 << 9).filter(|it| it.count_ones() == 4);
    let mut ran = 0;
    for order in orders {
        let mut disk: BTreeSet<String> = inputs.iter().cloned().collect();
        // The manifest as committed: the files it lists, and what it retired.
        let mut listed: BTreeSet<String> = inputs.iter().cloned().collect();
        let mut retired = Retired::default();
        let mut published = (listed.clone(), retired.clone());
        let (mut writer, mut reader) = (0, 0);
        // The writer's view of the pins (W2) and what it may delete (W3).
        let mut reapable = Vec::new();
        let mut scanned: Vec<Pinned> = Vec::new();
        // The reader's pin: made, and the generation it said; the manifest it read.
        let mut pin: Option<Pinned> = None;
        let mut read = published.clone();
        for step in 0..9 {
            if order & (1 << step) == 0 {
                match writer {
                    // The merge's output renamed into place, not yet listed.
                    0 => _ = disk.insert(merged.clone()),
                    // One commit: the output listed, its inputs unlisted and retired.
                    1 => {
                        listed.insert(merged.clone());
                        for input in &inputs {
                            listed.remove(input);
                        }
                        assert!(retired.retire(inputs.iter().cloned()));
                        published = (listed.clone(), retired.clone());
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
                    // Then forgotten, and that committed.
                    _ => {
                        retired.reaped(&reapable);
                        published = (listed.clone(), retired.clone());
                    }
                }
                writer += 1;
            } else {
                match reader {
                    0 => pin = Some(None),
                    1 => read = published.clone(),
                    2 => pin = Some(Some(read.1.generation)),
                    _ => {
                        for file in &read.0 {
                            assert!(
                                disk.contains(file),
                                "order {order:09b}: {file} was deleted under the reader"
                            );
                        }
                        let mut rows: Vec<u64> = read
                            .0
                            .iter()
                            .filter_map(|it| Name::parse(it))
                            .flat_map(|it| it.first..=it.last)
                            .collect();
                        rows.sort_unstable();
                        assert_eq!(
                            rows,
                            [1, 2, 3],
                            "order {order:09b}: the reader read {:?}",
                            read.0
                        );
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
        assert_eq!(disk, BTreeSet::from([merged.clone()]));
        ran += 1;
    }
    assert_eq!(ran, 126);
}

/// What a manifest may hold of retirement is checked, and a pin's files are told apart by
/// their names.
#[test]
fn retirement_checks_and_pins_parse() {
    let mut retired = Retired::default();
    assert!(
        retired
            .retire(["1-00000001.spans.parquet".to_owned(), "1-00000002.spans.parquet".to_owned()])
    );
    assert!(!retired.retire(["1-00000001.spans.parquet".to_owned()]), "a file is retired once");
    assert!(retired.retire(["0-00000003.folded.parquet".to_owned()]));
    assert_eq!(retired.generation, 2);
    assert_eq!(retired.check(), Ok(()));
    let ahead = Retired { generation: 1, files: vec![("a.spans.parquet".to_owned(), 2)] };
    assert!(ahead.check().is_err());
    let hidden = Retired { generation: 1, files: vec![(".a.spans.parquet".to_owned(), 1)] };
    assert!(hidden.check().is_err());

    assert_eq!(PinFile::parse("12-3.pin"), Some(PinFile::Lock { id: "12-3" }));
    assert_eq!(PinFile::parse("12-3.7.gen"), Some(PinFile::Said { id: "12-3", generation: 7 }));
    assert_eq!(PinFile::parse(".12-3.tmp"), Some(PinFile::Making { id: "12-3" }));
    assert_eq!(PinFile::parse("12.pin"), None);
    assert_eq!(PinFile::Said { id: "12-3", generation: 7 }.file_name(), "12-3.7.gen");
    assert_eq!(PinFile::Lock { id: "12-3" }.pid(), Some(12));
    assert_eq!(pinned(&["1-1", "2-1"], &[("2-1", 4)]), [None, Some(4)]);
}
