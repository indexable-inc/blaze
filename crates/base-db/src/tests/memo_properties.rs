//! The memo's answers against recomputation: over any run of edits, commits and reads, a query
//! and one that reads it give what the text they read gives now, and a read of a snapshot
//! no edit moved runs nothing.

use std::alloc::Global;

use hegel::generators as gs;

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

fn text(tc: &hegel::TestCase) -> String {
    tc.draw(gs::text().alphabet("ab\n").max_size(40))
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().max_value(7)) {
        0 | 1 => {
            let count = tc.draw(gs::integers::<u8>().min_value(1).max_value(3));
            let mut texts = Vec::new_in(Global);
            for _ in 0..count {
                texts.push((tc.draw(gs::integers().max_value(FILES - 1)), text(tc)));
            }
            Op::Set(texts)
        }
        2..7 => Op::Read {
            file: tc.draw(gs::integers().max_value(FILES - 1)),
            through: tc.draw(gs::booleans()),
        },
        _ => Op::Commit,
    }
}

/// The invariant: every answer of [`text_len`] and [`len_read_through`] is the length the
/// model's text gives, after any edits and commits; and an ask twice in one snapshot runs
/// nothing the second time.
fn gives_what_recomputing_gives(tc: &hegel::TestCase) {
    let mut db = test_db();
    let mut model = Vec::new_in(Global);
    let mut paths = Vec::new_in(Global);
    for file in 0..FILES {
        model.push(text(tc));
        paths.push(format!("/f{file}.rs"));
    }
    let mut files = Vec::new_in(Global);
    files.extend(paths.iter().zip(&model).map(|(path, text)| (path.as_str(), text.as_str())));
    load(&mut db, 0, &files);
    for op in tc.draw(gs::vecs(op()).max_size(60)) {
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

#[hegel::test(test_cases = 64)]
fn memos_give_what_recomputing_gives(tc: hegel::TestCase) {
    gives_what_recomputing_gives(&tc);
}
