//! Packs out of and into a store: the thin driver of `blaze_pack::import` and of the export
//! (docs/remote-cache.md, "Packs"). Lane packs-1112.
//!
//! The store is reached through [`Records`], which has two homes: [`Driven`] over a store
//! this process drives itself (a tool with no daemon on the home; the tests, on disk and on
//! the simulation), and `super::daemon::Held`, the daemon's own store while it runs,
//! through its handle and its loop. The export and the import are written once, over the
//! trait.
//!
//! [`export`] writes a selection's records as one pack under a directory: the manifest, the
//! body, the head (signed when a key is given). Two exports of one selection of one store
//! are the same bytes: the manifest is sorted by key, the body follows it, and nothing in
//! either says when or where it was made.
//!
//! [`import`] reads a pack from a [`Base`] into the store: the head (a trusted signature,
//! or the caller's word that the user named it unsigned), the manifest against the head,
//! then the pin of every key of the manifest under the manifest's pin (the store's own pin
//! form, `blaze_cas::collect::Pin`: the keys the name had before stay), then
//! `blaze_pack::import::run`, which checks every record before it is put. **The pin is
//! written before the first record**: no commit holds a record of the pack without its pin,
//! so no collection, at any time from the import on, can take one. A pin whose import then
//! fails names keys the store lacks, which costs their 32 bytes each and keeps nothing.

use std::{io::Write as _, path::Path};

use blaze_cas::{Key, Kind, Store};
use blaze_ids::ContentId;
use blaze_pack::{
    Id, Manifest, Record, Trust,
    head::{Head, Vouched},
    import::{self, Import, Summary, Verified},
    layout,
};

use super::{B3, Base, Body, Ed25519, admitted, compressed, id_of, key_of, sign};

/// Why an imported pack's records are pinned, as the store's pin says it.
pub(crate) const WHY: &str = "records imported from a pack, kept from the import on";
/// Tries at taking the scan that reads a store's pins, each after driving the store to rest.
const SCANS_WITHIN: u32 = 16;

/// A store as packs read and write it.
pub(crate) trait Records {
    /// The content hash kept under `key`, or none.
    fn content(&mut self, key: &Key) -> Result<Option<ContentId>, String>;
    /// `with` of the kind, content hash and bytes of the record of `key`, read verified;
    /// false when there is none.
    fn read(&mut self, key: &Key, with: &mut dyn FnMut(Kind, ContentId, &[u8])) -> Result<bool, String>;
    fn put(&mut self, key: Key, kind: Kind, bytes: &[u8]) -> Result<(), String>;
    /// The keys the pin `name` holds, or none when there is no such pin; the names of the
    /// pins there are, for the error.
    fn pinned(&mut self, name: &str) -> Result<Result<Vec<Key>, Vec<String>>, String>;
    /// Every key the store has a record of, each once, sorted.
    fn keys(&mut self) -> Result<Vec<Key>, String>;
    /// Adds `keys` to the pin `name`: what the name held stays.
    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), String>;
    /// What was put and pinned is durable, or on its way to be by the store's own commit.
    fn flush(&mut self) -> Result<(), String>;
}

/// A store and whatever drives its effects, in this process.
pub(crate) trait Driving {
    fn store(&mut self) -> &mut Store;
    /// What was put is durable when this returns.
    fn flush(&mut self) -> Result<(), String>;
}

impl Driving for blaze_cas::Driven {
    fn store(&mut self) -> &mut Store {
        blaze_cas::Driven::store(self)
    }

    fn flush(&mut self) -> Result<(), String> {
        blaze_cas::Driven::flush(self)
    }
}

/// `keys` added to the pin `name` of `store`, by the store's own rule
/// (`Store::pin_add`: merged with what the name holds, written only when a key is new, and
/// nothing written until the store's pins are read). An import's pin must be written before
/// its first record, so pins not yet read is the error here, never a pin left for later.
pub(crate) fn pin_grown(store: &mut Store, name: &str, keys: &[Key]) -> Result<(), String> {
    match store.pin_add(name, WHY, blaze_gc::record::now_ns(), keys) {
        Ok(blaze_cas::collect::PinAdded::Written(_) | blaze_cas::collect::PinAdded::Unchanged) => Ok(()),
        // alloc: error path
        Ok(blaze_cas::collect::PinAdded::NotRead) => Err(format!(
            "the pin `{name}` was not written: the store has not read its pins since it opened"
        )),
        Err(mismatch) => Err(format!("the pin `{name}` was not written: {mismatch:?}")), // alloc: error path
    }
}

/// The keys of the pin `name` of `store`, or the names of the pins it has.
pub(crate) fn pinned_in(store: &Store, name: &str) -> Result<Vec<Key>, Vec<String>> {
    match store.pins().find(|pin| pin.name == name) {
        Some(pin) => Ok(pin.keys.clone()), // clone: once an export
        None => Err(store.pins().map(|pin| pin.name.clone()).collect()), // alloc: error path
    }
}

/// Every key of `store`, each once, sorted.
pub(crate) fn keys_of(store: &Store) -> Vec<Key> {
    let mut keys = store.keys();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// A store this process drives, as [`Records`].
pub(crate) struct Driven<'a, D: Driving> {
    driving: &'a mut D,
    /// A planted bug for the simulation's control: the import's pin is not written.
    #[cfg(test)]
    pub(crate) plant_no_pin: bool,
}

impl<'a, D: Driving> Driven<'a, D> {
    pub(crate) fn new(driving: &'a mut D) -> Driven<'a, D> {
        Driven {
            driving,
            #[cfg(test)]
            plant_no_pin: false,
        }
    }

    /// Has the store's pins read (`Store::pins_read`): the scan that reads them is taken
    /// and run here (`Store::pins_take`, `pins_put`), as no daemon does it.
    fn read_pins(&mut self) -> Result<(), String> {
        for _ in 0..SCANS_WITHIN {
            let store = self.driving.store();
            if store.pins_read() {
                return Ok(());
            }
            match store.pins_take() {
                // The scan that reads the pins, run here: no period ends by it.
                Some(mut scan) => {
                    scan.run();
                    store.pins_put(scan);
                }
                // A commit or an index swap is in flight: driven to its end, then asked again.
                None => self.driving.flush()?,
            }
        }
        // alloc: error path
        Err(format!(
            "the store's pins were not read in {SCANS_WITHIN} tries (a commit or an index swap stayed in flight), so a pin written now could drop the keys its name had: nothing was changed"
        ))
    }
}

impl<D: Driving> Records for Driven<'_, D> {
    fn content(&mut self, key: &Key) -> Result<Option<ContentId>, String> {
        match self.driving.store().get(key) {
            Ok(entry) => Ok(entry.map(|entry| entry.content)),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn read(&mut self, key: &Key, with: &mut dyn FnMut(Kind, ContentId, &[u8])) -> Result<bool, String> {
        match self.driving.store().get(key) {
            Ok(Some(entry)) => {
                with(entry.kind, entry.content, entry.bytes);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&mut self, key: Key, kind: Kind, bytes: &[u8]) -> Result<(), String> {
        match self.driving.store().put(key, kind, bytes) {
            Ok(_) => Ok(()),
            Err(mismatch) => Err(differs(&mismatch)),
        }
    }

    fn pinned(&mut self, name: &str) -> Result<Result<Vec<Key>, Vec<String>>, String> {
        self.read_pins()?;
        Ok(pinned_in(self.driving.store(), name))
    }

    fn keys(&mut self) -> Result<Vec<Key>, String> {
        Ok(keys_of(self.driving.store()))
    }

    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), String> {
        #[cfg(test)]
        if self.plant_no_pin {
            return Ok(());
        }
        self.read_pins()?;
        pin_grown(self.driving.store(), name, keys)
    }

    fn flush(&mut self) -> Result<(), String> {
        self.driving.flush()
    }
}

pub(crate) fn differs(mismatch: &blaze_cas::Mismatch) -> String {
    // alloc: error path
    format!(
        "the store keeps {} under that key and was given {} (two values of one key)",
        Id(mismatch.kept.0),
        Id(mismatch.given.0)
    )
}

/// [`Records`] as an import puts into it. Its pin was written before the first put
/// ([`import`]), so the import core's own pin step, after the last, has nothing left to do.
struct Sink<'a, R: Records>(&'a mut R);

impl<R: Records> import::Store for Sink<'_, R> {
    fn look(&mut self, key: &Id) -> Result<Option<Id>, String> {
        Ok(self.0.content(&key_of(key))?.map(|content| Id(content.0)))
    }

    fn put(&mut self, record: Verified<'_>) -> Result<(), String> {
        let Some(kind) = Kind::from_code(record.kind()) else {
            return Err(format!("kind {} is no kind of this store", record.kind())); // alloc: error path
        };
        self.0.put(key_of(&record.key()), kind, record.bytes())
    }

    fn pin(&mut self, _: &str, _: &[Id]) -> Result<(), String> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        self.0.flush()
    }
}

/// What an export wrote.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Exported {
    pub pack: Id,
    pub records: usize,
    pub bytes: u64,
    pub signed: bool,
    /// Keys the selection named that the store has no record of: not in the pack.
    pub absent: usize,
}

fn written(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("writing {}: {error}", path.display()); // alloc: error path
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    let partial = path.with_extension("partial");
    std::fs::write(&partial, bytes).map_err(failed)?;
    std::fs::rename(&partial, path).map_err(failed)
}

/// Writes a selection of the store's records as a pack for `scope` under `out`: the
/// directory to give to a mirror, a peer or a release's upload as it is. `seed`: the
/// Ed25519 secret its head is signed with; without one the head is unsigned, which only an
/// import of the directory by hand takes.
///
/// The selection is the records the store's pin `pin` keeps: a key the pin names that the
/// store has no record of keeps nothing and is counted ([`Exported::absent`]); a record it
/// keeps of a kind that does not travel is refused naming the key (a pack is never written
/// with less than the records of its selection). With `all` it is every record of
/// the store that may travel ([`admitted`]: never a pin, never the daemon's image, machine
/// code only under a signature), and `pin` is the name the importing store pins them under:
/// everything the builds of this home used. `raw`: the body is written as it is, not as
/// compressed spans (a directory read in place gains nothing from compression).
pub(crate) fn export(
    records: &mut impl Records,
    pin: &str,
    scope: &str,
    out: &Path,
    seed: Option<&[u8; 32]>,
    all: bool,
    raw: bool,
) -> Result<Exported, String> {
    let admit = admitted(seed.is_some());
    let keys = match all {
        true => records.keys()?,
        false => match records.pinned(pin)? {
            Ok(keys) => keys,
            // alloc: error path
            Err(known) => return Err(format!("the store has no pin `{pin}` (its pins: {})", known.join(", "))),
        },
    };
    let mut listed = Vec::with_capacity(keys.len()); // alloc: once an export
    let mut absent = 0usize;
    for key in &keys {
        let mut found = None;
        records.read(key, &mut |kind, content, bytes| found = Some((kind, content, bytes.len() as u64)))?;
        match found {
            Some((kind, content, len)) if admit.contains(&kind.code()) => {
                listed.push(Record { key: id_of(key), kind: kind.code(), len, content: Id(content.0) });
            }
            // A pin names keys, not records: one it names that the store has no record of (a
            // slot that was never filled, a key pinned ahead of its import) keeps nothing
            // and is nothing to export. Counted and said, never hidden.
            None => absent += 1,
            // Of everything, what does not travel is not in the selection: that is its
            // definition.
            Some(_) if all => {}
            // Of a pin, every record it keeps is: one that may not travel refuses the pack.
            Some((kind, _, _)) => {
                // alloc: error path
                return Err(format!(
                    "the pin `{pin}` keeps the record of key {}, of kind `{}`, which does not travel ({}): nothing was written",
                    id_of(key),
                    kind.name(),
                    if kind == Kind::Machine { "machine code, in an unsigned pack: sign it with `--sign-key`" } else { "a pin or the daemon's image" }
                ));
            }
        }
    }
    let manifest = Manifest::new(pin, scope, listed).map_err(|refused| refused.to_string())?;
    let manifest_bytes = manifest.encode();
    let pack = blaze_pack::Hashes::manifest(&B3, &manifest_bytes);
    // The body, in the manifest's order, each record as the store reads it now, held to
    // what the manifest says of it.
    let body_at = out.join(layout::body_path(&pack));
    let failed = |error: std::io::Error| format!("writing {}: {error}", body_at.display()); // alloc: error path
    if let Some(dir) = body_at.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    let partial = body_at.with_extension("partial");
    let mut body = std::io::BufWriter::new(std::fs::File::create(&partial).map_err(failed)?);
    for record in manifest.records() {
        let mut wrote: Option<std::io::Result<()>> = None;
        records.read(&key_of(&record.key), &mut |_, content, bytes| {
            if Id(content.0) == record.content {
                wrote = Some(body.write_all(bytes));
            }
        })?;
        match wrote {
            Some(done) => done.map_err(failed)?,
            // alloc: error path
            None => return Err(format!("the record of key {} changed while it was exported", record.key)),
        }
    }
    body.into_inner().map_err(|error| failed(error.into_error()))?.sync_all().map_err(failed)?;
    std::fs::rename(&partial, &body_at).map_err(failed)?;
    // The body travels compressed, span by span, unless the caller keeps it raw: the store
    // read each record out uncompressed, so this is its one compression.
    if !raw {
        compressed(
            &body_at,
            &out.join(layout::frames_path(&pack)),
            &out.join(layout::spans_path(&pack)),
            manifest.body_len(),
        )?;
    }
    written(&out.join(layout::manifest_path(&pack)), &manifest_bytes)?;
    let mut head = Head::of(&manifest, pack, &manifest_bytes);
    if let Some(seed) = seed {
        let (signer, _) = sign(seed, b"")?;
        let (_, signature) = sign(seed, &head.message(&signer))?;
        head = head.signed_by(signer, signature);
    }
    let head_path = layout::head_path(scope, pin).map_err(|refused| refused.to_string())?;
    // The head last: a reader that finds it finds the manifest and the body it names.
    written(&out.join(head_path), &head.encode())?;
    Ok(Exported {
        pack,
        records: manifest.records().len(),
        bytes: manifest.body_len(),
        signed: head.signed.is_some(),
        absent,
    })
}

/// What an import of one pin did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Imported {
    pub pin: String,
    pub pack: Id,
    pub summary: Summary,
    pub vouched: Vouched,
}

/// Imports the pack of `pin` for `scope` at `base` into the store: none when the base has
/// no head of it. `unsigned`: the user named this base themselves and said to take a head
/// no one signed.
pub(crate) fn import(
    records: &mut impl Records,
    base: &Base,
    scope: &str,
    pin: &str,
    trust: &Trust,
    unsigned: bool,
) -> Result<Option<Imported>, String> {
    let Some(head) = base.head(scope, pin)? else { return Ok(None) };
    let named = |why: String| format!("the pack of pin `{pin}` at {}: {why}", base.named()); // alloc: error path
    let vouched = head.vouched(trust, &Ed25519, unsigned).map_err(|refused| named(refused.to_string()))?;
    let manifest = base.manifest(&head)?;
    let admit = admitted(matches!(vouched, Vouched::Signer(_)));
    let mut import = Import::new(manifest, &admit).map_err(|refused| named(refused.to_string()))?;
    // The pin first, of every key the manifest lists: from here on nothing of the pack is
    // ever in the store without it.
    let keys: Vec<Key> = import.manifest().records().iter().map(|it| key_of(&it.key)).collect(); // alloc: once an import
    records.pin(&head.pin, &keys).map_err(named)?;
    let mut body = Body::open(base, &head.pack, head.body_len).map_err(named)?;
    let summary = import::run(&mut import, &mut Sink(records), &mut body, &B3)
        .map_err(|refused| named(refused.to_string()))?;
    Ok(Some(Imported { pin: head.pin, pack: head.pack, summary, vouched }))
}
