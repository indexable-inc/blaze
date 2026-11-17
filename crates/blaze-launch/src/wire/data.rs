//! The run wire's [`super::run::Down::Data`]: the statics a record about to be published names,
//! each by its symbol and its datum record (blaze-code datum.rs), told after the `Declare` of the
//! slots that record and its data name and before its `Publish` (docs/v2/plan/design/run.md,
//! "Statics in the run").
//!
//! A static has no stub: a load through its word must find its bytes, so it is placed before
//! anything names it, never on a first call. One message is one batch: the program gives every
//! datum in it its address before it fills any, so a datum may name another of the same
//! message (a cycle of statics). The body is `{"Data": {"data": [{"record": bytes(32),
//! "symbol": bytes(32)}, ..]}}`, its keys in the order of their encodings.

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::run::Symbol;
use super::{List, Listed, RecordId, WireError, id, variant};

/// The variant's name in a body.
pub(super) const NAME: &str = "Data";

/// One static told: the symbol code records name it by, and its datum record in the store.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Datum {
    pub symbol: Symbol,
    pub record: RecordId,
}

impl Listed<'_> for Datum {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        // The keys in the order of their encodings: by length, then by bytes.
        to.map(2);
        to.text("record");
        to.bytes(&self.record.0);
        to.text("symbol");
        to.bytes(&self.symbol.0);
    }

    fn read(from: &mut Reader<'_>) -> Result<Datum, WireError> {
        from.map(2, "a static told, a map of 2")?;
        from.key("record")?;
        let record = RecordId(id(from, "a record id is 32 bytes")?);
        from.key("symbol")?;
        let symbol = Symbol(id(from, "a symbol is 32 bytes")?);
        Ok(Datum { symbol, record })
    }
}

/// Writes the variant's body: `{"Data": {"data": data}}`.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, data: List<'_, Datum>) {
    variant(to, NAME, 1);
    to.text("data");
    data.write(to);
}

/// Reads the variant's fields, the reader just past its name.
pub(super) fn read<'a>(from: &mut Reader<'a>) -> Result<List<'a, Datum>, WireError> {
    from.map(1, "Data, a map of 1")?;
    from.key("data")?;
    List::read(from)
}

#[cfg(test)]
mod tests {
    use super::super::run::{Down, Symbol};
    use super::super::{List, RecordId};
    use super::Datum;

    /// The message reads back as written, its statics in the parent's order; a frame cut
    /// anywhere is refused, never read as another message. Prints "run wire: Data round-trips
    /// with 2 statics; every cut refused".
    #[test]
    fn data_round_trips_and_a_cut_is_refused() {
        let data = [Datum { symbol: Symbol([9; 32]), record: RecordId([0xcd; 32]) }, Datum { symbol: Symbol([2; 32]), record: RecordId([0x11; 32]) }];
        let down = Down::Data { data: List::of(&data) };
        let mut out = std::vec::Vec::new();
        down.encode(8, &mut out).expect("Data encodes");
        let (item, took) = super::super::split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one frame");
        let (id, read) = Down::decode(item).expect("Data decodes");
        assert_eq!(id, 8);
        assert_eq!(read, down);
        let Down::Data { data: told } = read else { panic!("read as another variant: {read:?}") };
        let order: std::vec::Vec<u8> = told.iter().map(|it| it.symbol.0[0]).collect();
        assert_eq!(order, [9, 2], "the statics keep the parent's order");
        for cut in 0..item.len() {
            assert!(Down::decode(&item[..cut]).is_err(), "a Data cut to {cut} bytes was read");
        }
        println!("run wire: Data round-trips with {} statics; every cut refused", told.len());
    }
}
