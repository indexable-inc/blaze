//! The query engine's memos against recomputation: over any run of edits, reads, LRU evictions and
//! synthetic writes, a query and one that reads it give what the text they read gives now.

use crate::FileId;
use blaze_query::db::{Database as _, Durability};
use blaze_sim::{Config, Tape};

use crate::{Db, FileText};

const FILES: u32 = 4;

/// What `text_len`'s tag (`Query`, as it names none) may hold in the test's database.
const BUDGET: u64 = 16;

/// Weighed at 8 bytes a value against a budget of two ([`BUDGET`]), so most runs evict some.
#[blaze_query::tracked(returns(copy), lru, weigh = |_: &u32| 8)]
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
    Read {
        file: u32,
        through: bool,
    },
    /// The budget applies when a view opens: a write that changed nothing, of every kind.
    Evict,
    SyntheticWrite,
}

fn text(tape: &Tape) -> String {
    tape.text_from("ab\n", 0..=40, "text")
}

fn op(tape: &Tape) -> Op {
    let file = tape.int_in(0..=FILES - 1, "file");
    match tape.int_in(0..=7u8, "op") {
        0 | 1 => Op::Set(file, text(tape)),
        2..6 => Op::Read { file, through: tape.bool("through") },
        6 => Op::Evict,
        _ => Op::SyntheticWrite,
    }
}

fn gives_what_recomputing_gives(tape: &Tape) {
    let mut db = Db::for_tests();
    db.storage_mut().set_value_budget(blaze_mem::Tag::Query, Some(BUDGET));
    let mut model = Vec::new();
    for file in 0..FILES {
        let text = text(tape);
        db.set_file_text(FileId::from_raw(file), &text);
        model.push(text);
    }
    for op in tape.vec(0..=60, "ops", op) {
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

/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn memos_and_evictions_give_what_recomputing_gives() {
    blaze_sim::check(Config::cases(64), gives_what_recomputing_gives);
}
