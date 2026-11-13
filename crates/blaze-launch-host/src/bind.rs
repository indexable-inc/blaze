//! The bind record: a function's or static's launch record ([`crate::record`]) with, for each
//! place it numbers, what fills it, each callee and static by its own bind key. It is what
//! answers a launcher's first call of a function with no database, describe or template:
//! one `get` of the home's store under the function's [`bind_key`].
//!
//! # Layout
//!
//! Little-endian; a count before what it counts.
//!
//! ```text
//! "bzbind02" | u8 what (0 function, 1 static) | u32 n | n bytes (Made.bytes) | u64 len |
//! u32 align | [32] slot identity (zero for a static) | [32] code hash (zero for a static) |
//! u32 n | n Names | u32 n | n [32] anchors (sorted, each once)
//!
//! a Names is one of:
//!   u8 0 | [32] key | [32] slot identity | u8 has | [32] bind | u32 len | ask   (a function)
//!   u8 1 | [32] item | [32] instance | u8 has | [32] bind        (a static)
//!   u8 2 | u32 len | name                                       (the program's own symbol)
//! ```
//!
//! A function's `ask` is its portable ask (`HirFrontend::ask_bytes`): what a front end loaded
//! after a start reads back as the instance to make, when no bind record answers its first
//! call. Empty where the function has none (the allocator shim, an instance with an
//! environment, an item outside its home's closure).
//!
//! `Names` are one per numbered place of the record (`Place::numbered`), in its order: a
//! `Function` place is a `Function` or a `Symbol`, a `Static` place a `Static` or a `Symbol`.
//! The record's id is the BLAKE3 hash of its bytes, worked out again when it is read; what
//! [`Made::named`] and [`Made::symbols`] say is read from the names. [`decode`] refuses bytes
//! of another layout, a record that does not read, and names that do not fit its places, each
//! naming the offset.
//!
//! # The key
//!
//! [`bind_key`] covers every input of the record's bytes (rule 21):
//!
//! | input | covered by |
//! |---|---|
//! | the function's code, its debug name, its positions' lines, its callees and statics, its slot identity and code hash | `ask`: the key of its describe record (`Frontend::record_key`), which names every input of the describe |
//! | how it is compiled: the code generator's code identity, the target, the CPU, the profile | [`Under::policy`] (`Policy::id`, of `Toolchain`) |
//! | the file names its positions carry, of its home crate and every crate it loads | `naming` (`HirFrontend::naming_of`) |
//! | which of its `Extern` and `Weak` relocations are places the program's own symbol fills, and as what | [`Under::symbols`] ([`symbols_id`]) |
//! | the code that makes a launch record of a compiled function and lays out this one | [`Under::maker`] and [`BIND_DOMAIN`] |
//! | each callee's portable ask in the names | the callee's instance, which the describe names, written by the code `Under::maker`'s closure holds (hir-ty-codegen) |
//! | each callee's and static's bind key in the names | the same four of the callee: a callee's describe is named by this one's, its crates are among this one's, and the build's policy, symbols and maker are one |

use blaze_cas::Key;
use blaze_codegen::{InstanceKey, StaticId};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use blaze_launch::record::{Place, Record, What};
use blaze_launch::wire::RecordId;

use crate::program::Plant;
use crate::record::{Made, Named};

/// The domain of a bind key; another layout or another formula is another domain.
pub const BIND_DOMAIN: &str = "blaze v2 launch bind 2";

const MAGIC: &[u8; 8] = b"bzbind02";

const SLOT_DOMAIN: &str = "blaze v2 launch slot 1";

const SYMBOLS_DOMAIN: &str = "blaze v2 launch symbols 1";

/// What every bind record of one program's build is made under, besides each function's own
/// inputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Under {
    /// `Policy::id` of the build.
    pub policy: ContentId,
    /// The program's own symbols: [`symbols_id`] of its (name, function or static) pairs.
    pub symbols: ContentId,
    /// The code identity of what makes a launch record of a compiled function: blaze-cargo's
    /// record code (`blaze_codegen_code::RECORD`, `BLAZE_RECORD_CODE`), whose closure holds
    /// this crate and blaze-launch. Without it a record made by an older maker would be read
    /// as this one's.
    pub maker: ContentId,
}

/// The store key of the bind record of the function or static whose describe record is
/// under `ask`, whose crates the build names as `naming`, made `under` the build's policy,
/// symbols and record maker.
pub fn bind_key(ask: &Key, naming: &ContentId, under: &Under) -> Key {
    let mut hasher = Hasher::new(BIND_DOMAIN);
    hasher.id(&ask.0).id(naming).id(&under.policy).id(&under.symbols).id(&under.maker);
    Key(hasher.finish())
}

/// A function's slot identity: what it is across edits of its body (`identity`), with what a
/// caller's code assumes of it beyond its address (`abi`, `Described::abi`). A slot is a path
/// and an ABI (launcher.md, section 1): a function whose signature changes is another slot,
/// so the body of the old signature stays where the frames of its old callers call it.
pub fn slot_identity(identity: &ContentId, abi: &ContentId) -> ContentId {
    let mut hasher = Hasher::new(SLOT_DOMAIN);
    hasher.id(identity).id(abi);
    hasher.finish()
}

/// [`Under::symbols`] of the program's symbols: over its (name, function or static) pairs
/// sorted by name, each name length first. Which function or static a name is does not
/// shape a record (the reader resolves it); whether it is one, and of which kind, does.
pub fn symbols_id(symbols: &[(String, Names)]) -> ContentId {
    // alloc: a list of borrowed pairs, once a build
    let mut sorted: Vec<(&str, u8)> = symbols
        .iter()
        .map(|(name, names)| {
            let kind = match names {
                Names::Function { .. } => 0,
                Names::Static { .. } => 1,
                Names::Symbol(_) => 2,
            };
            (name.as_str(), kind)
        })
        .collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = Hasher::new(SYMBOLS_DOMAIN);
    hasher.update(&(sorted.len() as u64).to_le_bytes());
    for (name, kind) in sorted {
        hasher.update(&(name.len() as u64).to_le_bytes()).update(name.as_bytes()).update(&[kind]);
    }
    hasher.finish()
}

/// What fills one numbered place of a record.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a symbol table's copy of a name's place
pub enum Names {
    /// A function: its instance key in the build that made the record, its slot identity
    /// ([`slot_identity`]), its bind key when it may have a bind record, and its portable
    /// ask (`HirFrontend::ask_bytes`), empty when it has none.
    Function { key: InstanceKey, slot: ContentId, bind: Option<Key>, ask: Vec<u8> },
    /// A static, and its bind key when it may have a bind record.
    Static { id: StaticId, bind: Option<Key> },
    /// A place the program's own symbol fills: resolved by the reader, in the program's
    /// symbols.
    Symbol(String),
}

/// A record, bound: what a launcher takes for a slot or a static, and what a bind record
/// holds.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's copy
pub struct Bound {
    pub made: Made,
    /// The slot identity of the function ([`slot_identity`]); zero for a static.
    pub slot: ContentId,
    /// The code hash of the generic the function is an instance of; zero for a static.
    pub code: CodeHash,
    /// What fills each numbered place of `made`, in order.
    pub names: Vec<Names>,
}

/// The bytes of one `Names`, in the layout of the module's documentation.
pub(crate) fn put_names(out: &mut Vec<u8>, names: &Names) {
    let bind = |out: &mut Vec<u8>, bind: &Option<Key>| match bind {
        Some(key) => {
            out.push(1);
            out.extend_from_slice(&key.0.0);
        }
        None => {
            out.push(0);
            out.extend_from_slice(&[0; 32]);
        }
    };
    match names {
        Names::Function { key, slot, bind: of, ask } => {
            out.push(0);
            out.extend_from_slice(&key.0.0);
            out.extend_from_slice(&slot.0);
            bind(out, of);
            out.extend_from_slice(&(ask.len() as u32).to_le_bytes());
            out.extend_from_slice(ask);
        }
        Names::Static { id, bind: of } => {
            out.push(1);
            out.extend_from_slice(&id.0.0.0);
            out.extend_from_slice(&id.1.0);
            bind(out, of);
        }
        Names::Symbol(name) => {
            out.push(2);
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
    }
}

/// How many bytes [`put_names`] writes of `names`.
pub(crate) fn names_len(names: &Names) -> usize {
    match names {
        Names::Function { ask, .. } => 1 + 32 + 32 + 33 + 4 + ask.len(),
        Names::Static { .. } => 1 + 32 + 32 + 33,
        Names::Symbol(name) => 5 + name.len(),
    }
}

/// The bind record of `bound`, in the layout of the module's documentation.
pub fn encode(bound: &Bound) -> Vec<u8> {
    let made = &bound.made;
    let names: usize = bound.names.iter().map(names_len).sum();
    // alloc: the record's bytes, which the store keeps
    let mut out = Vec::with_capacity(8 + 1 + 4 + made.bytes.len() + 8 + 4 + 64 + 4 + names + 4 + 32 * made.anchors.len());
    out.extend_from_slice(MAGIC);
    out.push(u8::from(bound.slot == ContentId::default()));
    out.extend_from_slice(&(made.bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&made.bytes);
    out.extend_from_slice(&made.len.to_le_bytes());
    out.extend_from_slice(&made.align.to_le_bytes());
    out.extend_from_slice(&bound.slot.0);
    out.extend_from_slice(&bound.code.0.0);
    out.extend_from_slice(&(bound.names.len() as u32).to_le_bytes());
    for names in &bound.names {
        put_names(&mut out, names);
    }
    out.extend_from_slice(&(made.anchors.len() as u32).to_le_bytes());
    for anchor in &made.anchors {
        out.extend_from_slice(&anchor.0);
    }
    out
}

/// The bound record `bytes` hold, or why they are no bind record of this layout, naming the
/// offset.
pub fn decode(bytes: &[u8]) -> Result<Bound, String> {
    read(bytes, Plant::None)
}

/// A reader of a bind record or a start image (`crate::start`): each read names what it
/// reads and where, for a refusal.
pub(crate) struct Reader<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) at: usize,
    /// What the bytes are, for a refusal: "a bind record", "a start image".
    pub(crate) what: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        match self.bytes.get(self.at..self.at.saturating_add(len)) {
            Some(it) => {
                self.at += len;
                Ok(it)
            }
            None => Err(format!(
                // alloc: an error
                "{} of {} bytes ends at offset {} inside its {what} ({len} bytes from there)",
                self.what,
                self.bytes.len(),
                self.at
            )),
        }
    }

    pub(crate) fn u8(&mut self, what: &str) -> Result<u8, String> {
        Ok(self.take(1, what)?[0])
    }

    pub(crate) fn u32(&mut self, what: &str) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().expect("four bytes")))
    }

    pub(crate) fn u64(&mut self, what: &str) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().expect("eight bytes")))
    }

    pub(crate) fn id(&mut self, what: &str) -> Result<ContentId, String> {
        Ok(ContentId(self.take(32, what)?.try_into().expect("32 bytes")))
    }

    pub(crate) fn bind(&mut self) -> Result<Option<Key>, String> {
        let at = self.at;
        let has = self.u8("a name's bind flag")?;
        let key = self.id("a name's bind key")?;
        match has {
            0 if key == ContentId::default() => Ok(None),
            1 => Ok(Some(Key(key))),
            _ => Err(format!("{}'s bind flag at offset {at} is {has} with key {key:?}", self.what)), // alloc: an error
        }
    }

    pub(crate) fn refuse(&self, at: usize, what: std::fmt::Arguments<'_>) -> String {
        format!("{} of {} bytes, at offset {at}: {what}", self.what, self.bytes.len()) // alloc: an error
    }

    /// `len` bytes, UTF-8, refused by offset where they are not.
    pub(crate) fn str(&mut self, len: usize, what: &str) -> Result<&'a str, String> {
        let at = self.at;
        let bytes = self.take(len, what)?;
        core::str::from_utf8(bytes).map_err(|why| self.refuse(at, format_args!("its {what} is not UTF-8: {why}")))
    }
}

/// One `Names`, as [`put_names`] wrote it.
pub(crate) fn get_names(from: &mut Reader<'_>) -> Result<Names, String> {
    let tag = from.u8("a name's kind")?;
    Ok(match tag {
        0 => {
            let key = InstanceKey(from.id("a function's key")?);
            let slot = from.id("a function's slot identity")?;
            let bind = from.bind()?;
            let len = from.u32("a function's ask's length")? as usize;
            let ask = from.take(len, "a function's ask")?.to_vec(); // alloc: the ask, held by the name
            Names::Function { key, slot, bind, ask }
        }
        1 => {
            let item = ItemKey(from.id("a static's item")?);
            let instance = from.id("a static's instance")?;
            Names::Static { id: (item, instance), bind: from.bind()? }
        }
        2 => {
            let len = from.u32("a symbol's length")? as usize;
            Names::Symbol(from.str(len, "a symbol's name")?.to_owned()) // alloc: the name, held by the name
        }
        _ => return Err(from.refuse(from.at - 1, format_args!("a name of kind {tag}, which this layout has none of"))),
    })
}

/// [`decode`], with a planted bug of the tests' (`Plant::DropsStaticNames`) or none.
pub(crate) fn read(bytes: &[u8], plant: Plant) -> Result<Bound, String> {
    let mut from = Reader { bytes, at: 0, what: "a bind record" };
    let magic = from.take(8, "layout's name")?;
    if magic != MAGIC {
        return Err(from.refuse(
            0,
            format_args!(
                "its layout is {:?}, and this blaze reads {:?}",
                String::from_utf8_lossy(magic),
                String::from_utf8_lossy(MAGIC)
            ),
        ));
    }
    let what_at = from.at;
    let what = from.u8("kind")?;
    let len = from.u32("record's length")? as usize;
    let record_at = from.at;
    let record_bytes = from.take(len, "launch record")?;
    let placed = from.u64("placed length")?;
    let align_at = from.at;
    let align = from.u32("alignment")?;
    let slot = from.id("slot identity")?;
    let code = CodeHash(from.id("code hash")?);
    let count = from.u32("count of names")? as usize;
    // Each name is at least five bytes: no count past what the bytes can hold is believed.
    if count > bytes.len().saturating_sub(from.at) / 5 {
        return Err(from.refuse(from.at - 4, format_args!("it counts {count} names, more than its bytes hold")));
    }
    let mut names = Vec::with_capacity(count); // alloc: the bound record's names
    let mut at_of = Vec::with_capacity(count); // alloc: as above, for refusals
    for _ in 0..count {
        at_of.push(from.at);
        let names_one = get_names(&mut from)?;
        names.push(names_one);
    }
    let count = from.u32("count of anchors")? as usize;
    if count > bytes.len().saturating_sub(from.at) / 32 {
        return Err(from.refuse(from.at - 4, format_args!("it counts {count} anchors, more than its bytes hold")));
    }
    let mut anchors = Vec::with_capacity(count); // alloc: the bound record's anchors
    for _ in 0..count {
        let at = from.at;
        let anchor = from.id("an anchor")?;
        if anchors.last().is_some_and(|last| *last >= anchor) {
            return Err(from.refuse(at, format_args!("its anchors are not sorted, each once")));
        }
        anchors.push(anchor);
    }
    if from.at != bytes.len() {
        return Err(from.refuse(from.at, format_args!("{} bytes follow its last anchor", bytes.len() - from.at)));
    }

    // The launch record, and that what the names say fits it.
    let record = Record::decode(record_bytes)
        .map_err(|why| from.refuse(record_at, format_args!("its launch record does not read: {why}")))?;
    match (what, record.what) {
        (0, What::Code) if slot != ContentId::default() => {}
        (1, What::Static { .. }) if slot == ContentId::default() && code == CodeHash(ContentId::default()) => {}
        _ => {
            return Err(from.refuse(
                what_at,
                format_args!(
                    "it is of kind {what} with slot identity {slot:?}, and its launch record `{}` is {:?}",
                    record.name, record.what
                ),
            ));
        }
    }
    if record.align != align {
        return Err(from.refuse(
            align_at,
            format_args!("its alignment is {align}, and its launch record `{}`'s {}", record.name, record.align),
        ));
    }
    let numbered: Vec<Place<'_>> = record.places.iter().filter(Place::numbered).collect(); // alloc: as above
    if numbered.len() != names.len() {
        return Err(from.refuse(
            record_at,
            format_args!(
                "its launch record `{}` numbers {} places, and it names {}",
                record.name,
                numbered.len(),
                names.len()
            ),
        ));
    }
    let mut named = Vec::with_capacity(names.len()); // alloc: the made record's lists
    let mut symbols = Vec::new(); // alloc: as above
    for ((place, (names_one, at)), number) in numbered.iter().zip(names.iter().zip(&at_of)).zip(0u32..) {
        match (place, names_one) {
            (Place::Function, Names::Function { key, .. }) => named.push(Named::Function(*key)),
            (Place::Static, Names::Static { id, .. }) => named.push(Named::Static(*id)),
            (Place::Function | Place::Static, Names::Symbol(name)) => symbols.push((number, name.clone())), // clone: the made record's copy
            _ => {
                return Err(from.refuse(
                    *at,
                    format_args!("place {number} of `{}` is a {place:?}, and it names {names_one:?}", record.name),
                ));
            }
        }
    }
    let mut bound = Bound {
        made: Made {
            id: RecordId(*blake3::hash(record_bytes).as_bytes()),
            bytes: record_bytes.to_vec(), // alloc: the record's bytes, held by the bound record
            len: placed,
            align,
            named,
            symbols,
            anchors,
        },
        slot,
        code,
        names,
    };
    if plant == Plant::DropsStaticNames {
        bound.names.retain(|it| !matches!(it, Names::Static { .. }));
        bound.made.named.retain(|it| !matches!(it, Named::Static(_)));
    }
    Ok(bound)
}
