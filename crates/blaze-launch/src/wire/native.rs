//! The run wire's [`super::run::Down::Native`]: the parent names the program's native library
//! record and the Rust functions C code in it calls (port27/tempo-run/NATIVE.md 3.4 and 3.7;
//! piece W8).
//!
//! Sent once, first, before any `Declare` of the entry: the launcher stubs each pair's slot,
//! `dlopen`s the record, fills the library's word `i` with the entry thunk of `words[i]`'s slot
//! and runs the library's initializers before the entry. Each pair also declares its slot, as a
//! `Declare` pair does (the parent's number for the symbol, fixed for the run). The body is
//! `{"Native": {"words": [pair, ..], "library": bytes(32)}}`, its keys in the order of their
//! encodings (by length, then by bytes: "words" before "library").

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::run::Declared;
use super::{List, RecordId, WireError, id, variant};

/// The variant's name in a body.
pub(super) const NAME: &str = "Native";

/// Writes the variant's body: `{"Native": {"words": words, "library": library}}`.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, library: RecordId, words: List<'_, Declared>) {
    variant(to, NAME, 2);
    to.text("words");
    words.write(to);
    to.text("library");
    to.bytes(&library.0);
}

/// Reads the variant's fields, the reader just past its name.
pub(super) fn read<'a>(from: &mut Reader<'a>) -> Result<(RecordId, List<'a, Declared>), WireError> {
    from.map(2, "Native, a map of 2")?;
    from.key("words")?;
    let words = List::read(from)?;
    from.key("library")?;
    let library = RecordId(id(from, "a record id is 32 bytes")?);
    Ok((library, words))
}

#[cfg(test)]
mod tests {
    use super::super::List;
    use super::super::RecordId;
    use super::super::run::{Declared, Down, Symbol};
    use crate::Slot;

    /// The message reads back as written, from its own frame (the four-byte length stripped by
    /// `split`), its word pairs in the parent's order; a frame cut anywhere is refused, never
    /// read as another message. Prints "run wire: Native round-trips with 2 words; every cut
    /// refused".
    #[test]
    fn native_round_trips_and_a_cut_is_refused() {
        let pairs = [
            Declared { symbol: Symbol([7; 32]), slot: Slot(3) },
            Declared { symbol: Symbol([1; 32]), slot: Slot(900) },
        ];
        let down = Down::Native { library: RecordId([0xab; 32]), words: List::of(&pairs) };
        let mut out = std::vec::Vec::new();
        down.encode(5, &mut out).expect("Native encodes");
        let (item, took) = super::super::split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one frame");
        let (id, read) = Down::decode(item).expect("Native decodes");
        assert_eq!(id, 5);
        assert_eq!(read, down);
        let Down::Native { words, .. } = read else { panic!("read as another variant: {read:?}") };
        let order: std::vec::Vec<u32> = words.iter().map(|it| it.slot.0).collect();
        assert_eq!(order, [3, 900], "the word pairs keep the parent's order (word i is pair i)");
        for cut in 0..item.len() {
            assert!(Down::decode(&item[..cut]).is_err(), "a Native cut to {cut} bytes was read");
        }
        println!("run wire: Native round-trips with {} words; every cut refused", words.len());
    }
}
