use super::*;

fn flushes(build: u64, seqs: std::ops::RangeInclusive<u64>) -> impl Iterator<Item = Name> {
    seqs.map(move |seq| Name { build, table: 0, first: seq, last: seq, level: 0 })
}

#[test]
fn forty_flushes_make_two_merges_and_leave_eight() {
    let files: Vec<Name> = flushes(1, 1..=40).collect();
    let merges = plan(&files, None);
    assert_eq!(merges.len(), 2, "{merges:?}");
    assert_eq!(merges[0].inputs, (0..16).collect::<Vec<_>>());
    assert_eq!(merges[0].output, Name { build: 1, table: 0, first: 1, last: 16, level: 1 });
    assert_eq!(merges[1].inputs, (16..32).collect::<Vec<_>>());
    assert_eq!(merges[1].output, Name { build: 1, table: 0, first: 17, last: 32, level: 1 });
    assert_eq!(
        merges[0].output.file_name(),
        format!("1-00000001-00000016-1.{}.parquet", schema::TABLES[0].name)
    );
    assert_eq!(Name::parse(&merges[0].output.file_name()), Some(merges[0].output));
}

#[test]
fn a_builds_end_merges_each_of_its_tables_into_one_and_no_other_builds() {
    let mut files: Vec<Name> = flushes(2, 1..=3).collect();
    files.push(Name { build: 2, table: 0, first: 4, last: 19, level: 1 });
    files.extend(flushes(3, 20..=21));
    let merges = plan(&files, Some(2));
    assert_eq!(
        merges,
        [Merge {
            inputs: vec![0, 1, 2, 3],
            output: Name { build: 2, table: 0, first: 1, last: 19, level: 2 }
        }]
    );
    let alone = [Name { build: 2, table: 0, first: 1, last: 1, level: 0 }];
    assert!(plan(&alone, Some(2)).is_empty(), "a single file is never merged");
}

#[test]
fn a_merged_file_supersedes_the_lower_files_inside_its_range() {
    let mut files: Vec<Name> = flushes(1, 1..=3).collect();
    files.push(Name { build: 1, table: 0, first: 1, last: 2, level: 1 });
    files.push(Name { build: 1, table: 1, first: 1, last: 1, level: 0 });
    files.push(Name { build: 4, table: 0, first: 2, last: 2, level: 0 });
    assert_eq!(superseded(&files), [0, 1]);
}
