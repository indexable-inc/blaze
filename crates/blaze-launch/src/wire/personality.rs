//! The run wire's [`super::run::Down::Personality`]: the parent names the slot of the program's
//! `#[lang = "eh_personality"]` fn (port27/tempo-run/UNWIND.md, 3.3; piece P3).
//!
//! Sent once, after the `Declare` naming that fn and before any `Publish`: every placed record's
//! unwind entry names that slot's entry thunk as its personality routine (blaze-code eh.rs
//! `Placed::personality`; the thunk's address is fixed for the run, blaze-code place.rs). The
//! body is `{"Personality": {"slot": uint}}`, in the wire's deterministic encoding like every
//! other variant (one key, so its order is the only one).

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::{WireError, variant};
use crate::Slot;

/// The variant's name in a body.
pub(super) const NAME: &str = "Personality";

/// Writes the variant's body: `{"Personality": {"slot": slot}}`.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, slot: Slot) {
    variant(to, NAME, 1);
    to.text("slot");
    to.uint(u64::from(slot.0));
}

/// Reads the variant's fields, the reader just past its name.
pub(super) fn read(from: &mut Reader<'_>) -> Result<Slot, WireError> {
    from.map(1, "Personality, a map of 1")?;
    from.key("slot")?;
    Ok(Slot(from.uint32("a slot past the 32 bits a table has")?))
}

#[cfg(test)]
mod tests {
    use super::super::WireError;
    use super::super::run::Down;
    use crate::Slot;

    /// The message the parent sends once per run reads back as written, from its own frame (the
    /// envelope's four-byte length stripped by `split`); a frame cut anywhere is refused, never
    /// read as another message; and a `Personality` whose map holds a second key is refused at
    /// the map's head. Prints "run wire: Personality round-trips; every cut and a two-key body
    /// refused".
    #[test]
    fn personality_round_trips_and_a_cut_is_refused() {
        let mut out = std::vec::Vec::new();
        Down::Personality { slot: Slot(300) }.encode(9, &mut out).expect("Personality encodes");
        let (item, took) = super::super::split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one frame");
        let (id, down) = Down::decode(item).expect("Personality decodes");
        assert_eq!(id, 9);
        assert_eq!(down, Down::Personality { slot: Slot(300) });
        for cut in 0..item.len() {
            assert!(Down::decode(&item[..cut]).is_err(), "a Personality cut to {cut} bytes was read");
        }

        // The same envelope with a body `{"Personality": {"slot": 1, "tier": 0}}`: a second key
        // the schema does not have.
        let mut bad: std::vec::Vec<u8> = std::vec::Vec::new();
        {
            let mut to = super::super::cbor::Writer::new(&mut bad);
            to.map(3);
            to.text("id");
            to.uint(1);
            to.text("run");
            to.uint(u64::from(super::super::run::RUN_SCHEMA));
            to.text("body");
            super::super::variant(&mut to, super::NAME, 2);
            to.text("slot");
            to.uint(1);
            to.text("tier");
            to.uint(0);
        }
        assert!(
            matches!(Down::decode(&bad), Err(WireError::Type { want: "Personality, a map of 1", .. })),
            "a two-key Personality was not refused at its map's head: {:?}",
            Down::decode(&bad)
        );
        println!("run wire: Personality round-trips; every cut and a two-key body refused");
    }
}
