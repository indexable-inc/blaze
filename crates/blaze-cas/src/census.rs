//! What a store holds, counted ([`Store::census`]): bytes by record kind, by layer and in whole
//! files, from its index rows and its files' sizes alone. No value is read and nothing is
//! allocated a record: one walk of each tier's rows ([`crate::read::Look::each_row`]).
//!
//! What a record of a kind costs: `stored`, its payload as kept in a pack (compressed where it
//! is); `overhead`, what the store spends on it beside that: its header
//! ([`crate::RECORD_HEADER`]), the padding to the next record ([`disk::ALIGN`]) and, once the
//! index table holds it, its slot there (a control byte and a row). A set's leaf entries
//! (32 bytes an item, `sets::encode_leaf`) are payload of the [`Kind::Set`] record that holds
//! them: counting them against the items' kinds would read the leaves. A value kept as a file
//! of its own ([`disk::Form::External`]) keeps nothing in a pack but its header: its file is
//! in `whole_files`.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_core::FileName;

use crate::{
    Kind, Store,
    disk::{self, Form},
    index,
    read::{At, Found, Place},
};

/// One kind's records in the store's own directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindCensus {
    pub kind: Kind,
    pub records: u64,
    /// Their payloads as kept in packs, padding not included.
    pub stored: u64,
    /// Their values' lengths.
    pub raw: u64,
    /// Headers, padding and index slots: what keeping them costs beyond `stored`.
    pub overhead: u64,
}

/// What a store holds ([`Store::census`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Census {
    /// The store's own records, by kind, in [`Kind::ALL`]'s order.
    pub kinds: [KindCensus; Kind::ALL.len()],
    /// Each layer's (or other writer's) store read behind its own: its directory, its records
    /// and the bytes they take (their spans in packs, and their whole files).
    pub layers: Vec<(FileName, u64, u64)>,
    /// The store's own whole files (`file-*`, values kept as files of their own): how many,
    /// and their bytes.
    pub whole_files: (u64, u64),
}

/// The bytes one slot of an index table takes: its control byte and its row.
pub(crate) const INDEX_SLOT: u64 = 1 + index::ROW as u64;

impl Store {
    /// What the store holds: its records by kind, each layer's, and its whole files. Read from
    /// the index rows and the heads' file lengths, no value read; one walk of every row.
    pub fn census(&self) -> Census {
        self.census_of(|_| true)
    }

    /// [`Store::census`] of the own rows `counted` keeps: all of them, but in its test's
    /// planted bug.
    pub(crate) fn census_of(&self, counted: impl Fn(&Found<'_>) -> bool) -> Census {
        let look = self.look();
        let mut kinds = Kind::ALL
            .map(|kind| KindCensus { kind, records: 0, stored: 0, raw: 0, overhead: 0 });
        // alloc: the rows whose slot is not whole, which a census does not count; empty on a
        // sound store
        let mut bad = Vec::new();
        look.each_row(0, &mut bad, |_, found| {
            let Some(row) = row_of(&found).filter(|_| counted(&found)) else { return };
            let it = &mut kinds[row.kind.code() as usize];
            it.records += 1;
            it.stored += row.stored;
            it.raw += row.raw;
            it.overhead += row.span - row.stored + row.slot;
        });
        let own = &self.tiers[0];
        let live = own.externals.values().filter(|it| !it.dropped);
        let whole_files = live.fold((0, 0), |(files, bytes), it| (files + 1, bytes + it.len));
        // alloc: the census is the output, one row a layer
        let mut layers = Vec::with_capacity(self.tiers.len().saturating_sub(1));
        for at in 1..self.tiers.len() {
            let (mut records, mut bytes) = (0u64, 0u64);
            look.each_row(at, &mut bad, |_, found| {
                if let Some(row) = row_of(&found) {
                    records += 1;
                    bytes += row.span;
                }
            });
            let tier = &self.tiers[at];
            bytes += tier.externals.values().map(|it| it.len).sum::<u64>();
            // clone: the census is the output, one name a layer
            layers.push((tier.dir.clone(), records, bytes));
        }
        Census { kinds, layers, whole_files }
    }
}

/// What a census counts of one row.
struct Row {
    kind: Kind,
    stored: u64,
    raw: u64,
    /// Its header, payload and padding.
    span: u64,
    /// Its index slot's bytes, when the index table holds it.
    slot: u64,
}

fn row_of(found: &Found<'_>) -> Option<Row> {
    let (kind, form, stored, raw) = match found.place {
        Place::Pack(loc) => (loc.kind, loc.form, loc.stored, loc.raw),
        Place::Segment(seg) => (seg.kind, seg.form, seg.stored, seg.raw),
    };
    if form == Form::Dictionary {
        return None;
    }
    let span = disk::span_of(stored)?;
    let slot = match found.at {
        At::Slot(..) => INDEX_SLOT,
        At::Spot(..) => 0,
    };
    Some(Row { kind, stored, raw, span, slot })
}
