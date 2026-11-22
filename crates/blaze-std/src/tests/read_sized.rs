//! [`Dir::read_sized`](crate::fs::Dir::read_sized): a file whose length the caller knows, read
//! in ONE effect with no stat before it; a file that grew or shrank since its look is refused
//! by name (std.md, "End state: efficiency", piece 5).
//!
//! The control: the planted read of exactly `len` bytes (`read_sized_past` with 0 past) reads
//! the grown file cut at `len` and does not notice; the test sees it pass where the real one
//! refuses.

use blaze_core::Effect;

use super::{path, root, write_synced};
use crate::{fs::SizedError, io::Io, mem::Bytes};

/// Prints `read_sized: 1 effect (read_all: 2); grew and shrank refused; exactly len misses the growth`.
#[test]
fn a_sized_read_is_one_effect_and_refuses_a_file_that_grew() {
    let io = Io::sim(46);
    let cx = root(&io);
    let top = io.need_fs().expect("a file tree").root();
    let file = path("src/lib.rs");
    write_synced(&cx, &top, &file, b"fn f() {}\n").expect("the file is written");
    let looked = top.looks(&cx, [&file]).expect("its look").get(0).expect("one look");
    assert_eq!(looked.len, 10, "the look's length");

    // One effect, the bytes as they are.
    let mut into = Bytes::new_in(cx.frame(), "test read_sized");
    io.record();
    io.asked();
    let read = top.read_sized(&cx, &file, looked.len, &mut into).expect("the sized read");
    let sized = io.asked();
    assert_eq!((read, into.as_slice()), (10, b"fn f() {}\n".as_slice()));
    assert_eq!(sized.len(), 1, "one effect, no stat first: {sized:?}");
    assert!(matches!(sized[0], Effect::Read { at: 0, len: 11, .. }), "one read of len + 1: {:?}", sized[0]);
    into.clear();
    top.read_all(&cx, &file, &mut into).expect("the whole read");
    let whole = io.asked().len();
    assert_eq!(whole, 2, "read_all is a stat and a read");

    // The file grows between its look and its read: refused, naming it, `into` untouched.
    write_synced(&cx, &top, &file, b"fn f() {}\nfn g() {}\n").expect("the file grows");
    into.clear();
    match top.read_sized(&cx, &file, looked.len, &mut into) {
        Err(SizedError::Grew { file: named, len: 10 }) => assert_eq!(named.as_str(), "src/lib.rs"),
        other => panic!("a grown file was not refused: {other:?}"),
    }
    assert!(into.is_empty(), "nothing added on a refusal");

    // The planted bug: exactly `len` bytes asked, so the growth is not seen.
    let planted = top.read_sized_past(&cx, &file, looked.len, 0, &mut into);
    assert_eq!(planted.ok(), Some(10), "the planted read takes the grown file cut at len: the control's half");
    into.clear();

    // It shrinks: refused, saying how many bytes it has.
    write_synced(&cx, &top, &file, b"fn f(){}").expect("the file shrinks");
    match top.read_sized(&cx, &file, looked.len, &mut into) {
        Err(SizedError::Shrank { len: 10, read: 8, .. }) => {}
        other => panic!("a shrunk file was not refused: {other:?}"),
    }
    std::println!("read_sized: 1 effect (read_all: {whole}); grew and shrank refused; exactly len misses the growth");
}
