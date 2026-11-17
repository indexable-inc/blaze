//! The kept roots of a crate kept as a record and read back instead of found: what
//! [`super::kept`] finds at module and impl level, and the bodies whose blocks declare a kept
//! item (its owners), so a crate served from its records lowers those bodies alone, never
//! every body of it. Kept under `hir_def::deps::kept_key` of the key the crate's image is
//! filed under, beside its impl tables, and served and stored where they are.
//!
//! The record: [`MAGIC`], then what `hir_def::nameres::frozen::Enc` writes (the files the ids
//! name, with their content IDs, then the body): a count before what it counts, each a
//! variable-length integer, of the functions (each a `FunctionId`), the statics (each a
//! `StaticId`) and the owners (each a `DefWithBodyId`), every id written by its location as
//! `hir-ty/src/method_resolution/frozen.rs` writes an impl's items.

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, StaticId,
    deps::{Image, ImageBuf, ImageStore, kept_key},
    nameres::frozen::{Dec, Enc, Mode, Unreadable},
};
use std::alloc::Global;

type R<T = ()> = Result<T, String>;

/// The first bytes of a record of this layout. A record of another is refused by name: its
/// key is another (`hir_def::deps::KEPT_DOMAIN` names the layout), so one is only found
/// under this key by a bug.
pub const MAGIC: &[u8; 8] = b"bzkept01";

/// What a crate's kept roots are read from: the record, held where the store keeps it (a few
/// ids a crate), and the key it was found under. Equal when the keys are: the store gives the
/// bytes of a key only after checking them.
#[derive(Clone)]
pub struct Record {
    pub key: ContentId,
    pub bytes: ImageBuf,
}

impl PartialEq for Record {
    fn eq(&self, other: &Record) -> bool {
        self.key == other.key
    }
}

impl Eq for Record {}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("key", &self.key)
            .field("len", &self.bytes.bytes().len())
            .finish()
    }
}

/// The kind of a crate's slot holding its kept-roots [`Record`]. A crate's record is read from
/// its own slot (`hir_def::deps::given`) only: no map of every crate's is held, the driver
/// keeps what it gave.
pub struct KeptSlot;

/// The fact a crate's kept-roots slot is read by: the slot's id, of the crate's own stable id,
/// and the record's key (the store checks a key's bytes, so equal keys are equal records).
pub fn kept_fact(krate_id: ContentId, record: &Record) -> blaze_memo::Fact {
    let id = blaze_ids::Hasher::new("hir-ty-codegen kept roots slot").id(&krate_id).finish();
    blaze_memo::Fact { id, digest: record.key }
}

/// Has finding the kept roots of each crate of `entries` (crates of `edit`'s last commit) read
/// its record from the snapshot `edit` commits, given in the crate's own slot ([`KeptSlot`])
/// with the fact it is read by ([`kept_fact`]). Called by the driver in its edit of the world;
/// how many crates were given (a record equal to the one held gives nothing).
pub fn give(
    edit: &mut base_db::Edit<'_>,
    entries: impl IntoIterator<Item = (Crate, blaze_memo::Fact, Record)>,
) -> usize {
    hir_def::deps::given::give_each::<KeptSlot, Record>(edit, entries)
}

/// The record `krate` was given, when it is the record of the image the crate is read from
/// now: a record is its image's (its key is made from the image's), so a crate whose image
/// went, or was not read in [`Mode::Serve`], finds its kept roots from source.
fn record(db: &Db, krate: Crate) -> Option<&Record> {
    let record = krate.given::<KeptSlot, Record>(db)?;
    let image = hir_def::deps::image_of(db, krate)?;
    (image.mode() == Mode::Serve && kept_key(image.key()) == record.key).then_some(record)
}

/// What a kept-roots record says ([`decode`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Listed {
    /// The functions kept at module and impl level, in the order found.
    pub functions: Vec<FunctionId>,
    /// The statics kept at module level, in the order found.
    pub statics: Vec<StaticId>,
    /// The bodies whose blocks declare a kept item, in the order found.
    pub owners: Vec<DefWithBodyId>,
}

/// What the record of `krate` says, when the crate is read from one. A record that does not
/// read stops the query that asked with [`Unreadable`], naming the crate and why; nothing is
/// found from source in its place.
pub(super) fn served(db: &Db, krate: Crate) -> Option<Listed> {
    let record = record(db, krate)?;
    // The value is the record's, found by content: its key is the fact read.
    blaze_memo::read(
        blaze_memo::Database::memo(db),
        blaze_memo::Fact { id: record.key, digest: record.key },
    );
    match decode(db, krate, record.bytes.bytes()) {
        Ok(listed) => Some(listed),
        Err(why) => {
            // alloc: an error's crate name, once
            let name =
                krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_string());
            Unreadable { krate, name, kind: "kept roots", why }.throw()
        }
    }
}

/// The record of what `krate` keeps ([`super::kept`]): its lists at module and impl level and
/// its owners. Refuses an id the frozen encoding cannot write, naming it.
pub fn encode(db: &Db, krate: Crate) -> R<Vec<u8>> {
    let (functions, statics) = super::at_module_level(db, krate);
    encode_lists(db, krate, &functions, &statics, &super::kept(db, krate).owners)
}

/// The record of `functions`, `statics` and `owners`, ids of `krate`.
pub(super) fn encode_lists(
    db: &Db,
    krate: Crate,
    functions: &[FunctionId],
    statics: &[StaticId],
    owners: &[DefWithBodyId],
) -> R<Vec<u8>> {
    let name = || krate.named(db);
    let mut enc = Enc::new(db, krate);
    enc.usize(functions.len())?;
    for (at, &func) in functions.iter().enumerate() {
        // alloc: error path
        enc.def(func.into()).map_err(|why| format!("kept roots of {}: function {at}: {why}", name()))?;
    }
    enc.usize(statics.len())?;
    for (at, &value) in statics.iter().enumerate() {
        // alloc: error path
        enc.def(value.into()).map_err(|why| format!("kept roots of {}: static {at}: {why}", name()))?;
    }
    enc.usize(owners.len())?;
    for (at, &owner) in owners.iter().enumerate() {
        // alloc: error path
        enc.body_owner(owner).map_err(|why| format!("kept roots of {}: owner {at}: {why}", name()))?;
    }
    let body = enc.finish()?;
    // alloc: the record, once per crate per store
    let mut out = Vec::with_capacity(MAGIC.len() + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&body);
    Ok(out)
}

/// What the record `bytes` of `krate` says, its files checked against this database's. An
/// error names the crate, the list and the index where it stopped, or the layout the bytes
/// are of.
pub fn decode(db: &Db, krate: Crate, bytes: &[u8]) -> R<Listed> {
    let name = || krate.named(db);
    let Some(body) = bytes.strip_prefix(MAGIC.as_slice()) else {
        let head = &bytes[..bytes.len().min(MAGIC.len())];
        // alloc: error path
        return Err(format!(
            "kept roots of {}: a record of another layout ({:?}, this one is {:?})",
            name(),
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(MAGIC)
        ));
    };
    let at = |list: &'static str, at: usize| {
        // alloc: error path
        move |why: String| format!("kept roots of {}: {list} {at}: {why}", name())
    };
    let mut dec = Dec::new_checked(db, krate, body)
        .map_err(|why| format!("kept roots of {}: its files: {why}", name()))?;
    let count = |dec: &mut Dec<'_>, list: &'static str| {
        // alloc: error path
        dec.len().map_err(|why| format!("kept roots of {}: the count of {list}: {why}", name()))
    };
    let n = count(&mut dec, "functions")?;
    // alloc: the record's lists, once per crate per revision; at most what is left to read
    let mut functions = Vec::with_capacity(n.min(body.len()));
    for index in 0..n {
        match dec.def().map_err(at("function", index))? {
            ModuleDefId::FunctionId(func) => functions.push(func),
            // alloc: error path
            other => return Err(at("function", index)(format!("not a function: {other:?}"))),
        }
    }
    let n = count(&mut dec, "statics")?;
    // alloc: as above
    let mut statics = Vec::with_capacity(n.min(body.len()));
    for index in 0..n {
        match dec.def().map_err(at("static", index))? {
            ModuleDefId::StaticId(value) => statics.push(value),
            // alloc: error path
            other => return Err(at("static", index)(format!("not a static: {other:?}"))),
        }
    }
    let n = count(&mut dec, "owners")?;
    // alloc: as above
    let mut owners = Vec::with_capacity(n.min(body.len()));
    for index in 0..n {
        owners.push(dec.body_owner().map_err(at("owner", index))?);
    }
    dec.finish().map_err(|why| format!("kept roots of {}: {why}", name()))?;
    Ok(Listed { functions, statics, owners })
}

/// The kept-roots record `images` keeps of each crate of `given` (a crate and the image the
/// driver gives it in the snapshot it builds) that is read from its image in [`Mode::Serve`]:
/// what the driver gives with [`give`] in the same snapshot, beside the impl tables, so the
/// build that lists a served crate's roots lowers only the bodies its record names. A crate
/// with no record is left out: its slot is empty in that snapshot. In `given`'s order, which
/// is the order the store is asked in.
///
/// A crate with an image and no record in the store finds its kept roots from source, by
/// the bodies whose text can say one, and [`store`] then writes the record. A record that is
/// found and does not read stops the query that reads it, naming the crate. `db` names the
/// crates in an error.
pub fn find<'a>(
    db: &Db,
    given: impl IntoIterator<Item = (Crate, &'a Image)>,
    // dyn: the driver's store, one call per crate per serve
    images: &dyn ImageStore,
    mode: Mode,
) -> R<Vec<(Crate, Record)>> {
    let mut found = Vec::new_in(Global);
    for (krate, image) in given {
        if mode != Mode::Serve || image.mode() != Mode::Serve {
            continue;
        }
        let at = kept_key(image.key());
        let bytes = images.get(&at).map_err(|why| {
            // alloc: error text
            format!("reading the kept roots of crate {} (key {at:?}): {why}", krate.named(db))
        })?;
        if let Some(bytes) = bytes {
            found.push((krate, Record { key: at, bytes }));
        }
    }
    Ok(found)
}

/// Writes to `images` the kept-roots record of each of `crates`, a crate with the key its
/// image is filed under, that is not read from its record already: what the next database's
/// [`find`] finds. Called once the crates are resolved, where their impl tables are written.
/// Gives the crates whose record does not encode, with why: such a crate has none in the
/// store and finds its kept roots from source in every database, which the caller says.
pub fn store(
    db: &Db,
    crates: &[(Crate, ContentId)],
    // dyn: the driver's store, one call per crate per front end stored
    images: &dyn ImageStore,
) -> R<Vec<(Crate, String)>> {
    let mut refused = Vec::new(); // alloc: none until a crate is refused
    for &(krate, filed) in crates {
        if record(db, krate).is_some_and(|it| it.key == kept_key(filed)) {
            continue;
        }
        match encode(db, krate) {
            Ok(bytes) => images.put(kept_key(filed), bytes).map_err(|why| {
                // alloc: error text
                format!("keeping the kept roots of crate {}: {why}", krate.named(db))
            })?,
            Err(why) => refused.push((krate, why)),
        }
    }
    Ok(refused)
}
