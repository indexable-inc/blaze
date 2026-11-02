use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use blaze_alloc_count::assert_at_most;
use blaze_ids::rng::SplitMix64;

use crate::{Bytes, Text, ascii_prefix, check, text_of};

/// The check against std's on bytes weighted to where it could differ: long ASCII runs that
/// cross 64-byte chunks, then a multibyte char, a bad byte, or a char cut by the end.
#[test]
fn the_check_is_stds() {
    const PIECES: [&[u8]; 9] =
        [b"a", b"fn x() {}\n", b"\xc3\xa9", b"\xe2\x82\xac", b"\xf0\x9f\xa6\x80", b"\x80", b"\xc3", b"\xff", b"\xed\xa0\x80"];
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..4000 {
        let mut bytes = vec![b'x'; rng.index(300)];
        for _ in 0..rng.index(4) {
            let piece = PIECES[rng.index(PIECES.len())];
            let at = rng.index(bytes.len() + 1);
            bytes.splice(at..at, piece.iter().copied());
        }
        assert_eq!(check(&bytes), std::str::from_utf8(&bytes).map(drop), "{bytes:?}");
        let ascii = ascii_prefix(&bytes);
        assert!(ascii % 64 == 0 && bytes[..ascii].is_ascii());
    }
}

#[test]
fn the_ascii_prefix_stops_at_the_chunk_with_a_high_byte() {
    let mut bytes = vec![b'a'; 200];
    assert_eq!(ascii_prefix(&bytes), 192);
    bytes[130] = 0xc3;
    assert_eq!(ascii_prefix(&bytes), 128);
    bytes[0] = 0x80;
    assert_eq!(ascii_prefix(&bytes), 0);
    assert_eq!(ascii_prefix(b""), 0);
}

fn heap(bytes: &[u8]) -> (Arc<[u8]>, Bytes) {
    let shared: Arc<[u8]> = Arc::from(bytes);
    let bytes = Bytes::from(Arc::clone(&shared));
    (shared, bytes)
}

/// Making, cloning and lending text allocates nothing: it is the bytes it was made from.
#[test]
fn text_allocates_nothing() {
    let (_, bytes) = heap("fn main() {}\n".repeat(100).as_bytes());
    let text = assert_at_most(0, "text_of", || text_of(bytes.clone())).expect("UTF-8");
    let shared = assert_at_most(0, "Text::shared", || text.shared()).expect("a heap text");
    let again = assert_at_most(0, "Text::from(Arc<str>)", || Text::from(Arc::clone(&shared)));
    assert!(again.ptr_eq(&text));
    let clone = assert_at_most(0, "Text::clone", || text.clone());
    assert_at_most(0, "Text::from_static", || Text::from_static("fn f() {}"));
    assert_at_most(0, "Text::default", Text::default);
    assert_at_most(0, "deref", || assert_eq!(clone.len(), text.as_str().len()));
}

/// Heap text is shared as its own `Arc`, one count more; equality, hashing and formatting are
/// the `str`'s.
#[test]
fn a_heap_text_is_shared_as_its_own_arc() {
    let (shared, bytes) = heap("fn é() {}\n".as_bytes());
    let text = text_of(bytes).expect("UTF-8");
    let arc = text.shared().expect("a heap text");
    assert_eq!(arc.as_ptr(), shared.as_ptr());
    assert_eq!(&*arc, "fn é() {}\n");
    assert_eq!(Arc::strong_count(&shared), 3, "the test's, the text's and the `Arc<str>`'s");
    drop(arc);
    assert_eq!(Arc::strong_count(&shared), 2);

    let other = text_of(heap(b"fn \xc3\xa9() {}\n").1).unwrap();
    assert_eq!(text, other);
    assert!(!text.ptr_eq(&other));
    assert_eq!(text, "fn é() {}\n");
    let hash = |it: &dyn Fn(&mut DefaultHasher)| {
        let mut hasher = DefaultHasher::new();
        it(&mut hasher);
        hasher.finish()
    };
    assert_eq!(hash(&|h| text.hash(h)), hash(&|h| "fn é() {}\n".hash(h)));
    assert_eq!(format!("{text}|{text:?}"), "fn é() {}\n|\"fn é() {}\\n\"");
    assert_eq!(text.as_str(), "fn é() {}\n");
}

#[test]
fn text_shares_the_bytes() {
    let (shared, bytes) = heap(b"fn main() {}\n");
    let text = text_of(bytes).expect("UTF-8");
    assert_eq!(&*text, "fn main() {}\n");
    assert_eq!(text.as_ptr(), shared.as_ptr());
    assert_eq!(Arc::strong_count(&shared), 2);
    drop(text);
    assert_eq!(Arc::strong_count(&shared), 1);
}

/// An `Arc<str>` becomes text with no copy, and back.
#[test]
fn an_arc_str_is_text_as_it_is() {
    let arc: Arc<str> = "struct S;\n".into();
    let text = Text::from(Arc::clone(&arc));
    assert_eq!(text.as_ptr(), arc.as_ptr());
    assert_eq!(Arc::strong_count(&arc), 2);
    assert!(Arc::ptr_eq(&text.shared().expect("a heap text"), &arc));
    assert_eq!(Text::from(String::from("struct S;\n")), text);
    assert_eq!(Text::from("struct S;\n"), text);
}

#[test]
fn bytes_that_are_not_utf8_say_where() {
    let (shared, bytes) = heap(b"ok\xff\xfe");
    let error = text_of(bytes).unwrap_err();
    assert_eq!(error.valid_up_to(), 2);
    assert_eq!(Arc::strong_count(&shared), 1, "a refused text drops its count");
}

/// Held bytes are the owner's range, kept alive by it; they have no `Arc` of their own.
#[test]
fn held_bytes_keep_their_owner() {
    let owner: Arc<Vec<u8>> = Arc::new(b"head fn f() {} tail".to_vec());
    let range: *const [u8] = &owner[5..14];
    // SAFETY: the `Vec` is never written or dropped while `owner`'s counts live, and the
    // `Bytes` holds one.
    let bytes = unsafe { Bytes::held(range, Arc::clone(&owner) as Arc<dyn Send + Sync>) };
    assert_eq!(Arc::strong_count(&owner), 2);
    assert_eq!(&*bytes, b"fn f() {}");
    assert!(bytes.shared().is_none());
    assert_eq!(format!("{bytes:?}"), "Bytes(9 bytes, held)");
    let text = assert_at_most(0, "text_of held bytes", || text_of(bytes.clone())).expect("UTF-8");
    assert!(text.shared().is_none(), "no `Arc<str>` holds a range");
    assert_eq!(text, "fn f() {}");
    assert_eq!(text.as_ptr(), owner[5..].as_ptr());
    drop((bytes, text));
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[test]
fn static_bytes_are_neither_shared_nor_held() {
    const TEXT: Text = Text::from_static("const C: u8 = 1;");
    assert_eq!(TEXT, "const C: u8 = 1;");
    assert!(TEXT.shared().is_none());
    assert_eq!(format!("{:?}", TEXT.bytes()), "Bytes(16 bytes, static)");
    assert_eq!(Text::default(), "");
    assert_eq!(Bytes::default(), Bytes::from_static(b""));
}

#[test]
fn bytes_and_text_go_between_threads_and_stay_small() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Bytes>();
    send_sync::<Text>();
    assert!(size_of::<Text>() <= 40, "{}", size_of::<Text>());
}
