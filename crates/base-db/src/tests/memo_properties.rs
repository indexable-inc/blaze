//! The memo's answers against recomputation: over any run of edits, commits and reads, a query
//! and one that reads it give what the text they read gives now, and a read of a snapshot
//! no edit moved runs nothing.

use std::alloc::Global;

use blaze_sim::{Config, Tape};

use super::{EditDb, load, ran, test_db};
use crate::{Db, FileId, FileText};

const FILES: u32 = 4;

#[blaze_memo::tracked(returns(copy))]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

/// Reads [`text_len`]: what a query above another answers after the one below reran.
#[blaze_memo::tracked(returns(copy))]
fn len_read_through(db: &Db, text: FileText) -> u32 {
    text_len(db, text) * 3 + 1
}

#[derive(Debug)]
enum Op {
    /// Texts set in one edit, committed.
    Set(Vec<(u32, String), Global>),
    Read { file: u32, through: bool },
    /// A commit of an edit that wrote nothing.
    Commit,
}

fn text(tape: &Tape) -> String {
    tape.text_from("ab\n", 0..=40, "text")
}

fn op(tape: &Tape) -> Op {
    match tape.int_in(0..=7u8, "op") {
        0 | 1 => {
            let count = tape.int_in(1..=3u8, "texts");
            let mut texts = Vec::new_in(Global);
            for _ in 0..count {
                texts.push((tape.int_in(0..=FILES - 1, "file"), text(tape)));
            }
            Op::Set(texts)
        }
        2..7 => Op::Read { file: tape.int_in(0..=FILES - 1, "file"), through: tape.bool("through") },
        _ => Op::Commit,
    }
}

/// The invariant: every answer of [`text_len`] and [`len_read_through`] is the length the
/// model's text gives, after any edits and commits; and an ask twice in one snapshot runs
/// nothing the second time.
fn gives_what_recomputing_gives(tape: &Tape) {
    let mut db = test_db();
    let mut model = Vec::new_in(Global);
    let mut paths = Vec::new_in(Global);
    for file in 0..FILES {
        model.push(text(tape));
        paths.push(format!("/f{file}.rs"));
    }
    let mut files = Vec::new_in(Global);
    files.extend(paths.iter().zip(&model).map(|(path, text)| (path.as_str(), text.as_str())));
    load(&mut db, 0, &files);
    for op in tape.vec(0..=60, "ops", op) {
        match op {
            Op::Set(texts) => {
                db.commit_with(|edit| {
                    for (file, text) in &texts {
                        edit.set_file_text(FileId::from_raw(*file), text);
                    }
                });
                for (file, text) in texts {
                    model[file as usize] = text;
                }
            }
            Op::Commit => {
                let committed = db.commit_with(|_| {});
                assert!(committed.dropped.is_empty(), "an empty edit dropped {:?}", committed.dropped);
            }
            Op::Read { file, through } => {
                let text = db.file_text(FileId::from_raw(file));
                let len = model[file as usize].len() as u32;
                let read = |db: &Db| match through {
                    true => len_read_through(db, text),
                    false => text_len(db, text),
                };
                let want = if through { len * 3 + 1 } else { len };
                assert_eq!(read(&db), want, "file {file}");
                ran(&db, "text_len");
                assert_eq!(read(&db), want, "file {file}, asked again");
                assert_eq!(ran(&db, "text_len"), 0, "file {file}: a hit in one snapshot reran");
            }
        }
    }
}

/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn memos_give_what_recomputing_gives() {
    blaze_sim::check(Config::cases(64), gives_what_recomputing_gives);
}
