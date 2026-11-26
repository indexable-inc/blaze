//! Sans-IO [`TermRegistry`] state machine for PUA symbols and entity metadata.

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    ApiError, EntityId, EntityKind, EntityMeta, MAX_ENTITIES, MAX_SYMBOLS, PUA_END, PUA_START,
    ServerMeta, SymbolRecord, SymbolSource, format_u64_pair, is_valid_symbol_name, protocol,
};

const PUA_SPAN: usize = (PUA_END - PUA_START + 1) as usize;
const PUA_BITMAP_WORDS: usize = PUA_SPAN.div_ceil(64);

/// Pure sans-IO registry of custom PUA symbols and entity metadata bindings.
#[derive(Clone, Debug)]
pub struct TermRegistry<'a> {
    alloc: Alloc<'a>,
    revision: u64,
    symbols: AVec<'a, SymbolRecord<'a>>,
    entities: AVec<'a, EntityMeta<'a>>,
}

impl PartialEq for TermRegistry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.symbols == other.symbols
            && self.entities == other.entities
    }
}

impl Eq for TermRegistry<'_> {}

impl<'a> TermRegistry<'a> {
    /// Create an empty registry backed by `alloc`.
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        Self {
            alloc,
            revision: 0,
            symbols: Vec::new_in(alloc),
            entities: Vec::new_in(alloc),
        }
    }

    /// Allocator backing this registry's stored records.
    pub const fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Monotonic revision counter bumped on every mutating operation.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// All currently registered symbols.
    pub fn symbols(&self) -> &[SymbolRecord<'a>] {
        self.symbols.as_slice()
    }

    /// All currently bound entities.
    pub fn entities(&self) -> &[EntityMeta<'a>] {
        self.entities.as_slice()
    }

    /// Look up a registered symbol by its slug name.
    pub fn symbol_by_name(&self, name: &str) -> Option<&SymbolRecord<'a>> {
        self.symbols.iter().find(|s| s.name.as_str() == name)
    }

    /// Look up a registered symbol by its assigned PUA codepoint.
    pub fn symbol_by_codepoint(&self, cp: u32) -> Option<&SymbolRecord<'a>> {
        self.symbols.iter().find(|s| s.codepoint == cp)
    }

    /// Look up bound metadata for `(kind, id)`.
    pub fn entity(&self, kind: EntityKind, id: &str) -> Option<&EntityMeta<'a>> {
        self.entities.iter().find(|e| e.id.kind == kind && e.id.id.as_str() == id)
    }

    /// Zero-allocation lookup for a `File` entity identified by `(dev, ino)`.
    pub fn entity_file(&self, dev: u64, ino: u64) -> Option<&EntityMeta<'a>> {
        let mut buf = [0u8; 48];
        let key = format_u64_pair(dev, ino, &mut buf);
        self.entity(EntityKind::File, key)
    }

    /// Register or update a symbol by slug name, assigning or validating its PUA codepoint.
    pub fn register_symbol(
        &mut self,
        name: &str,
        requested_cp: Option<u32>,
        width_cells: u8,
        source: SymbolSource<'a>,
    ) -> Result<&SymbolRecord<'a>, ApiError<'a>> {
        if !is_valid_symbol_name(name) {
            return Err(ApiError::InvalidField {
                field: "name",
                reason: String::from_str_in(
                    "must be 1..=96 bytes matching [a-zA-Z0-9._:-]",
                    self.alloc,
                ),
            });
        }
        validate_symbol_source(&source, self.alloc)?;

        let width_cells = width_cells.clamp(1, 2);
        let existing_idx = self.symbols.iter().position(|s| s.name.as_str() == name);

        if existing_idx.is_none() && self.symbols.len() >= MAX_SYMBOLS {
            return Err(ApiError::CapacityExceeded { kind: "symbols", max: MAX_SYMBOLS });
        }

        let codepoint = match requested_cp {
            Some(cp) => {
                if !(PUA_START..=PUA_END).contains(&cp) {
                    return Err(ApiError::CodepointOutOfRange { codepoint: cp });
                }
                if let Some((owner_idx, owner)) =
                    self.symbols.iter().enumerate().find(|(_, s)| s.codepoint == cp)
                    && Some(owner_idx) != existing_idx
                {
                    return Err(ApiError::CodepointInUse {
                        codepoint: cp,
                        owner: String::from_str_in(owner.name.as_str(), self.alloc),
                    });
                }
                cp
            }
            None => match existing_idx {
                Some(idx) => self.symbols[idx].codepoint,
                None => self.lowest_unused_codepoint().ok_or(ApiError::CapacityExceeded {
                    kind: "pua_codepoints",
                    max: PUA_SPAN,
                })?,
            },
        };

        self.revision = self.revision.saturating_add(1);
        let record_rev = self.revision;
        let record = SymbolRecord {
            name: String::from_str_in(name, self.alloc),
            codepoint,
            width_cells,
            source: source.clone_in(self.alloc),
            revision: record_rev,
        };

        let idx = match existing_idx {
            Some(idx) => {
                self.symbols[idx] = record;
                idx
            }
            None => {
                let idx = self.symbols.len();
                self.symbols.push(record);
                idx
            }
        };
        Ok(&self.symbols[idx])
    }

    /// Remove a registered symbol by name. Returns `true` if it existed.
    pub fn unregister_symbol(&mut self, name: &str) -> bool {
        let Some(idx) = self.symbols.iter().position(|s| s.name.as_str() == name) else {
            return false;
        };
        self.symbols.remove_shift(idx);
        self.revision = self.revision.saturating_add(1);
        true
    }

    /// Bind or update metadata for an entity identified by `(meta.id.kind, meta.id.id)`.
    pub fn bind_entity(&mut self, meta: EntityMeta<'a>) -> Result<&EntityMeta<'a>, ApiError<'a>> {
        if meta.id.id.is_empty() {
            return Err(ApiError::InvalidField {
                field: "id",
                reason: String::from_str_in("entity id must not be empty", self.alloc),
            });
        }

        let existing_idx = self
            .entities
            .iter()
            .position(|e| e.id.kind == meta.id.kind && e.id.id.as_str() == meta.id.id.as_str());

        if existing_idx.is_none() && self.entities.len() >= MAX_ENTITIES {
            return Err(ApiError::CapacityExceeded { kind: "entities", max: MAX_ENTITIES });
        }

        self.revision = self.revision.saturating_add(1);
        let mut stored = meta.clone_in(self.alloc);
        normalize_attrs(&mut stored.attrs);
        stored.revision = self.revision;

        let idx = match existing_idx {
            Some(idx) => {
                self.entities[idx] = stored;
                idx
            }
            None => {
                let idx = self.entities.len();
                self.entities.push(stored);
                idx
            }
        };
        Ok(&self.entities[idx])
    }

    /// Remove a bound entity by `(kind, id)`. Returns `true` if it existed.
    pub fn unbind_entity(&mut self, kind: EntityKind, id: &str) -> bool {
        let Some(idx) = self
            .entities
            .iter()
            .position(|e| e.id.kind == kind && e.id.id.as_str() == id)
        else {
            return false;
        };
        self.entities.remove_shift(idx);
        self.revision = self.revision.saturating_add(1);
        true
    }

    /// Handle a JSON-RPC request frame. Returns `(json_response, mutated)`. Never panics.
    pub fn apply_json(
        &mut self,
        raw: &[u8],
        meta: ServerMeta<'_>,
        alloc: Alloc<'a>,
    ) -> (AString<'a>, bool) {
        protocol::apply_json(self, raw, meta, alloc)
    }

    /// Handle an inbound OSC 7770 payload (JSON or compact `;`-separated `k=v` shorthand).
    pub fn apply_osc_7770(
        &mut self,
        payload: &[u8],
        meta: ServerMeta<'_>,
        alloc: Alloc<'a>,
    ) -> crate::OscOutcome<'a> {
        protocol::apply_osc_7770(self, payload, meta, alloc)
    }

    /// Encode the full registry state into a deterministic, canonical JSON snapshot.
    pub fn encode_snapshot(&self, alloc: Alloc<'a>) -> AString<'a> {
        protocol::encode_snapshot(self, alloc)
    }

    /// Decode a canonical JSON snapshot into a validated [`TermRegistry`].
    pub fn decode_snapshot(raw: &[u8], alloc: Alloc<'a>) -> Result<Self, ApiError<'a>> {
        protocol::decode_snapshot(raw, alloc)
    }

    /// Internal constructor used by `decode_snapshot` to preserve exact revisions after validation.
    pub(crate) fn from_validated_parts(
        alloc: Alloc<'a>,
        revision: u64,
        symbols: AVec<'a, SymbolRecord<'a>>,
        entities: AVec<'a, EntityMeta<'a>>,
    ) -> Self {
        Self { alloc, revision, symbols, entities }
    }

    fn lowest_unused_codepoint(&self) -> Option<u32> {
        let mut used = [0u64; PUA_BITMAP_WORDS];
        for sym in self.symbols.iter() {
            if let Some(offset) = sym.codepoint.checked_sub(PUA_START) {
                let idx = usize::try_from(offset).ok()?;
                if idx < PUA_SPAN {
                    used[idx / 64] |= 1u64 << (idx % 64);
                }
            }
        }
        for (word_idx, &word) in used.iter().enumerate() {
            if word != u64::MAX {
                let bit = word.trailing_ones();
                let offset = word_idx * 64 + usize::try_from(bit).ok()?;
                if offset < PUA_SPAN {
                    return u32::try_from(offset).ok().map(|o| PUA_START + o);
                }
            }
        }
        None
    }
}

/// Sort attributes deterministically by key (stable, zero-alloc, `#![no_std]`) and deduplicate
/// keeping the last value per key.
pub(crate) fn normalize_attrs(attrs: &mut AVec<'_, (AString<'_>, AString<'_>)>) {
    if attrs.len() <= 1 {
        return;
    }
    // Stable insertion sort preserves relative order of duplicate keys without allocation.
    for i in 1..attrs.len() {
        let mut j = i;
        while j > 0 && attrs[j].0.as_str() < attrs[j - 1].0.as_str() {
            attrs.swap(j, j - 1);
            j -= 1;
        }
    }
    let mut write = 0usize;
    let mut read = 0usize;
    let len = attrs.len();
    while read < len {
        let mut next = read + 1;
        while next < len && attrs[next].0.as_str() == attrs[read].0.as_str() {
            next += 1;
        }
        let last = next - 1;
        if write != last {
            attrs.swap(write, last);
        }
        write += 1;
        read = next;
    }
    attrs.truncate(write);
}

pub(crate) fn validate_symbol_source<'a>(
    source: &SymbolSource<'a>,
    alloc: Alloc<'a>,
) -> Result<(), ApiError<'a>> {
    match source {
        SymbolSource::Svg { markup, .. } => {
            if markup.as_str().trim().is_empty() {
                return Err(ApiError::InvalidField {
                    field: "svg",
                    reason: String::from_str_in("svg markup must not be empty", alloc),
                });
            }
            Ok(())
        }
        SymbolSource::FontGlyph { font_data_b64, codepoint, .. } => {
            if char::from_u32(*codepoint).is_none() {
                return Err(ApiError::InvalidField {
                    field: "glyph_codepoint",
                    reason: blaze_own::format_in!(
                        alloc,
                        "0x{codepoint:X} is not a valid Unicode scalar value"
                    ),
                });
            }
            if let Some(b64) = font_data_b64 {
                validate_standard_base64(b64.as_str(), "font_data_b64", alloc)?;
            }
            Ok(())
        }
        SymbolSource::Png { data_b64 } => {
            validate_standard_base64(data_b64.as_str(), "data_b64", alloc)
        }
        SymbolSource::Text { text } => {
            if text.is_empty() {
                return Err(ApiError::InvalidField {
                    field: "text",
                    reason: String::from_str_in("text symbol must not be empty", alloc),
                });
            }
            Ok(())
        }
    }
}

/// Zero-allocation canonical base64 validation using `blaze_base64::STANDARD`.
pub(crate) fn validate_standard_base64<'a>(
    input: &str,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<(), ApiError<'a>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Err(ApiError::InvalidBase64 {
            field,
            reason: String::from_str_in("base64 payload must not be empty", alloc),
        });
    }
    if bytes.len() % 4 != 0 {
        return Err(ApiError::InvalidBase64 {
            field,
            reason: blaze_own::format_in!(
                alloc,
                "length {} is not a multiple of 4",
                bytes.len()
            ),
        });
    }

    let prefix_len = bytes.len() - 4;
    let mut scratch = [0u8; 48];
    let mut offset = 0usize;
    while offset < prefix_len {
        let end = (offset + 64).min(prefix_len);
        if let Err(err) = blaze_base64::STANDARD_NO_PAD.decode_into(&bytes[offset..end], &mut scratch)
        {
            return Err(ApiError::InvalidBase64 {
                field,
                reason: blaze_own::format_in!(alloc, "{err} (chunk offset {offset})"),
            });
        }
        offset = end;
    }

    let mut tail_scratch = [0u8; 3];
    if let Err(err) = blaze_base64::STANDARD.decode_into(&bytes[prefix_len..], &mut tail_scratch) {
        return Err(ApiError::InvalidBase64 {
            field,
            reason: blaze_own::format_in!(alloc, "{err} (at tail offset {prefix_len})"),
        });
    }
    Ok(())
}

/// Helper to validate and construct a `File` entity id for RPC operations.
pub(crate) fn resolve_entity_id<'a>(
    kind: EntityKind,
    id: Option<&str>,
    dev: Option<u64>,
    ino: Option<u64>,
    alloc: Alloc<'a>,
) -> Result<EntityId<'a>, ApiError<'a>> {
    match (id, dev, ino) {
        (Some(raw_id), Some(d), Some(i)) if kind == EntityKind::File => {
            if raw_id.is_empty() {
                return Err(ApiError::InvalidField {
                    field: "id",
                    reason: String::from_str_in("entity id must not be empty", alloc),
                });
            }
            let expected = EntityId::file(d, i, alloc);
            if expected.id.as_str() != raw_id {
                return Err(ApiError::InvalidField {
                    field: "id",
                    reason: String::from_str_in(
                        "provided `id` does not match `dev:ino`",
                        alloc,
                    ),
                });
            }
            Ok(expected)
        }
        (Some(raw_id), _, _) => {
            if raw_id.is_empty() {
                return Err(ApiError::InvalidField {
                    field: "id",
                    reason: String::from_str_in("entity id must not be empty", alloc),
                });
            }
            Ok(EntityId::new(kind, raw_id, alloc))
        }
        (None, Some(d), Some(i)) if kind == EntityKind::File => Ok(EntityId::file(d, i, alloc)),
        (None, Some(_), None) | (None, None, Some(_)) if kind == EntityKind::File => {
            Err(ApiError::InvalidField {
                field: "dev",
                reason: String::from_str_in("file entity requires both `dev` and `ino`", alloc),
            })
        }
        _ => Err(ApiError::InvalidField {
            field: "id",
            reason: String::from_str_in(
                "missing `id` (or `dev` + `ino` for `file` entities)",
                alloc,
            ),
        }),
    }
}
