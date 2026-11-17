//! The run wire's [`super::run::Down::ThreadLocals`]: the parent names the run's thread-local
//! descriptor and its ender (run.md, "Statics in the run", step 4; THREADS.md D2).
//!
//! The descriptor is the CHILD's: it holds the run's pthread key and where a thread's table
//! lives, facts of one process, so no content-keyed datum can carry it. The parent tells its
//! symbol (the `Space::Static` target every record touching a thread-local names, blaze-ladder
//! symbols.rs `tls_desc_symbol`) and the ender's (the key's destructor, blaze-ladder
//! tls_index.rs `tls_ender_symbol`) once, before the first `Publish` of a record naming the
//! descriptor. The child then makes the descriptor and the key itself; the ender is a fn like
//! any other, numbered by the child and asked for at its first call (a thread's exit). The
//! body is `{"ThreadLocals": {"ender": bytes32, "descriptor": bytes32}}`, the keys in the
//! order of their encodings.

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::run::Symbol;
use super::{WireError, id, variant};

/// The variant's name in a body.
pub(super) const NAME: &str = "ThreadLocals";

/// Writes the variant's body.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, descriptor: Symbol, ender: Symbol) {
    variant(to, NAME, 2);
    to.text("ender");
    to.bytes(&ender.0);
    to.text("descriptor");
    to.bytes(&descriptor.0);
}

/// Reads the variant's fields, the reader just past its name: (descriptor, ender).
pub(super) fn read(from: &mut Reader<'_>) -> Result<(Symbol, Symbol), WireError> {
    from.map(2, "ThreadLocals, a map of 2")?;
    from.key("ender")?;
    let ender = Symbol(id(from, "a symbol is 32 bytes")?);
    from.key("descriptor")?;
    let descriptor = Symbol(id(from, "a symbol is 32 bytes")?);
    Ok((descriptor, ender))
}

#[cfg(test)]
mod tests {
    use super::super::WireError;
    use super::super::run::{Down, Symbol};

    /// The message reads back as written from its own frame; a frame cut anywhere is refused,
    /// never read as another message; a misspelt key is refused at that key.
    /// Prints "run wire: ThreadLocals round-trips; every cut and a misspelt key refused".
    #[test]
    fn thread_locals_round_trips_and_a_cut_is_refused() {
        let told = Down::ThreadLocals { descriptor: Symbol([0xd5; 32]), ender: Symbol([0xe7; 32]) };
        let mut out = std::vec::Vec::new();
        told.encode(4, &mut out).expect("ThreadLocals encodes");
        let (item, took) = super::super::split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one frame");
        let (id, down) = Down::decode(item).expect("ThreadLocals decodes");
        assert_eq!((id, down), (4, told));
        for cut in 0..item.len() {
            assert!(Down::decode(&item[..cut]).is_err(), "a ThreadLocals cut to {cut} bytes was read");
        }

        // The first key misspelt (`endes`, the same length): refused at that key.
        let mut bad = item.to_vec();
        let at = bad.windows(5).position(|w| w == b"ender").expect("the frame holds the key `ender`");
        bad[at + 4] = b's';
        let read = Down::decode(&bad);
        assert!(matches!(read, Err(WireError::Key { want: "ender", .. })), "a misspelt key was not refused at that key: {read:?}");
        println!("run wire: ThreadLocals round-trips; every cut and a misspelt key refused");
    }
}
