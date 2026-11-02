//! The query engine's memos against recomputation: over any run of edits, reads, LRU evictions and
//! synthetic writes, a query and one that reads it give what the text they read gives now.

use hegel::{Hegel, Settings, TestCase};
use hegel::generators as gs;
use blaze_query::db::{Database as _, Durability};
use crate::FileId;

use crate::{Db, FileText};

const FILES: u32 = 4;

/// Held to two memos, so most runs evict some.
#[blaze_query::tracked(returns(copy), lru = 2)]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

/// Not held to a count: kept while what it read may be evicted under it.
#[blaze_query::tracked(returns(copy))]
fn len_read_through(db: &Db, text: FileText) -> u32 {
    text_len(db, text) * 3 + 1
}

#[derive(Debug)]
enum Op {
    Set(u32, String),
    Read { file: u32, through: bool },
    /// The `lru` caps apply when a view opens: a write that changed nothing, of every kind.
    Evict,
    SyntheticWrite,
}

fn text(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("ab\n").max_size(40))
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let file = tc.draw(gs::integers().max_value(FILES - 1));
    match tc.draw(gs::integers::<u8>().max_value(7)) {
        0 | 1 => Op::Set(file, text(tc)),
        2..6 => Op::Read { file, through: tc.draw(gs::booleans()) },
        6 => Op::Evict,
        _ => Op::SyntheticWrite,
    }
}

fn gives_what_recomputing_gives(tc: TestCase) {
    let mut db = Db::for_tests();
    let mut model = Vec::new();
    for file in 0..FILES {
        let text = text(&tc);
        db.set_file_text(FileId::from_raw(file), &text);
        model.push(text);
    }
    for op in tc.draw(gs::vecs(op()).max_size(60)) {
        match op {
            Op::Set(file, text) => {
                db.set_file_text(FileId::from_raw(file), &text);
                model[file as usize] = text;
            }
            Op::Read { file, through } => {
                let text = db.file_text(FileId::from_raw(file));
                let len = model[file as usize].len() as u32;
                if through {
                    assert_eq!(len_read_through(&db, text), len * 3 + 1, "file {file}");
                } else {
                    assert_eq!(text_len(&db, text), len, "file {file}");
                }
            }
            Op::Evict => db.storage_mut().synthetic_write(Durability::HIGH),
            Op::SyntheticWrite => db.storage_mut().synthetic_write(Durability::LOW),
        }
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn memos_and_evictions_give_what_recomputing_gives() {
    run_cases(64, gives_what_recomputing_gives);
}
