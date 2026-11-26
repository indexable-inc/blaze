//! JSON-RPC, OSC 7770, and canonical snapshot codecs for [`TermRegistry`].

use blaze_data::{Int, Map, Value};
use blaze_json::{BytesAs, KeyOrder, Limits, Style, WriteOptions, read_in, to_string_in};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    ApiError, EntityId, EntityKind, EntityMeta, MAX_ENTITIES, MAX_MESSAGE_BYTES, MAX_SYMBOLS,
    OscOutcome, PROTOCOL_VERSION, PUA_END, PUA_START, ServerMeta, SymbolRecord, SymbolSource,
    TermRegistry, encode_osc_7770_json, format_hex_color, is_valid_symbol_name,
    parse_codepoint_str, parse_hex_color, parse_strict_u64,
    registry::{normalize_attrs, resolve_entity_id, validate_symbol_source},
};

const COMPACT_INSERTION: WriteOptions = WriteOptions {
    style: Style::Compact,
    keys: KeyOrder::Insertion,
    bytes: BytesAs::Refuse,
};

fn json_limits() -> Limits {
    Limits { most_bytes: MAX_MESSAGE_BYTES, ..Limits::SHIPPED }
}

fn encode_json<'a>(val: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> AString<'a> {
    match to_string_in(val, COMPACT_INSERTION, alloc) {
        Ok(s) => s,
        Err(_) => unreachable!("protocol values never contain Value::Bytes or non-finite floats"),
    }
}

fn encode_error<'a>(err: &ApiError<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut map = Map::with_capacity_in(3, alloc);
    insert_bool(&mut map, "ok", false, alloc);
    insert_str(&mut map, "error", err.message_in(alloc).as_str(), alloc);
    insert_str(&mut map, "code", err.code(), alloc);
    encode_json(&Value::Map(map), alloc)
}

pub(crate) fn apply_json<'a>(
    reg: &mut TermRegistry<'a>,
    raw: &[u8],
    meta: ServerMeta<'_>,
    alloc: Alloc<'a>,
) -> (AString<'a>, bool) {
    if raw.len() > MAX_MESSAGE_BYTES {
        let err = ApiError::TooLarge { len: raw.len(), max: MAX_MESSAGE_BYTES };
        return (encode_error(&err, alloc), false);
    }
    let val = match read_in(raw, json_limits(), alloc) {
        Ok(v) => v,
        Err(e) => {
            let err = ApiError::InvalidJson(blaze_own::format_in!(alloc, "{e}"));
            return (encode_error(&err, alloc), false);
        }
    };
    let Some(map) = val.as_map() else {
        let err = ApiError::InvalidJson(String::from_str_in("top-level value must be an object", alloc));
        return (encode_error(&err, alloc), false);
    };
    apply_map(reg, map, meta, alloc)
}

pub(crate) fn apply_osc_7770<'a>(
    reg: &mut TermRegistry<'a>,
    payload: &[u8],
    meta: ServerMeta<'_>,
    alloc: Alloc<'a>,
) -> OscOutcome<'a> {
    let trimmed = trim_ascii_bytes(payload);
    if trimmed.len() > MAX_MESSAGE_BYTES {
        let err = ApiError::TooLarge { len: trimmed.len(), max: MAX_MESSAGE_BYTES };
        return OscOutcome {
            mutated: false,
            reply_osc: None,
            response_json: encode_error(&err, alloc),
        };
    }
    if trimmed.is_empty() {
        let err = ApiError::InvalidField {
            field: "payload",
            reason: String::from_str_in("OSC 7770 payload must not be empty", alloc),
        };
        return OscOutcome {
            mutated: false,
            reply_osc: None,
            response_json: encode_error(&err, alloc),
        };
    }

    let (map, want_reply) = if trimmed.first() == Some(&b'{') {
        let val = match read_in(trimmed, json_limits(), alloc) {
            Ok(v) => v,
            Err(e) => {
                let err = ApiError::InvalidJson(blaze_own::format_in!(alloc, "{e}"));
                return OscOutcome {
                    mutated: false,
                    reply_osc: None,
                    response_json: encode_error(&err, alloc),
                };
            }
        };
        let Some(map) = val.into_map() else {
            let err =
                ApiError::InvalidJson(String::from_str_in("top-level value must be an object", alloc));
            return OscOutcome {
                mutated: false,
                reply_osc: None,
                response_json: encode_error(&err, alloc),
            };
        };
        let want_reply = match opt_bool(&map, "reply", alloc) {
            Ok(b) => b.unwrap_or(false),
            Err(err) => {
                return OscOutcome {
                    mutated: false,
                    reply_osc: None,
                    response_json: encode_error(&err, alloc),
                };
            }
        };
        (map, want_reply)
    } else {
        match parse_compact_osc_kv(trimmed, alloc) {
            Ok(parsed) => parsed,
            Err(err) => {
                return OscOutcome {
                    mutated: false,
                    reply_osc: None,
                    response_json: encode_error(&err, alloc),
                };
            }
        }
    };

    let (response_json, mutated) = apply_map(reg, &map, meta, alloc);
    let reply_osc = if want_reply {
        Some(encode_osc_7770_json(response_json.as_str(), alloc))
    } else {
        None
    };
    OscOutcome { mutated, reply_osc, response_json }
}

fn apply_map<'a>(
    reg: &mut TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    meta: ServerMeta<'_>,
    alloc: Alloc<'a>,
) -> (AString<'a>, bool) {
    let op = match req_str(map, "op", alloc) {
        Ok(op) => op,
        Err(err) => return (encode_error(&err, alloc), false),
    };

    let result: Result<(Value<Alloc<'a>>, bool), ApiError<'a>> = match op {
        "ping" => Ok((build_ping_response(reg.revision(), alloc), false)),
        "info" => Ok((build_info_response(reg, meta, alloc), false)),
        "symbol.register" => handle_symbol_register(reg, map, alloc),
        "symbol.unregister" => handle_symbol_unregister(reg, map, alloc),
        "symbol.get" => handle_symbol_get(reg, map, alloc),
        "symbol.list" => Ok((build_symbol_list_response(reg, alloc), false)),
        "entity.bind" => handle_entity_bind(reg, map, alloc),
        "entity.unbind" => handle_entity_unbind(reg, map, alloc),
        "entity.get" => handle_entity_get(reg, map, alloc),
        "entity.list" => handle_entity_list(reg, map, alloc),
        other => Err(ApiError::UnknownOp(String::from_str_in(other, alloc))),
    };

    match result {
        Ok((val, mutated)) => (encode_json(&val, alloc), mutated),
        Err(err) => (encode_error(&err, alloc), false),
    }
}

fn build_ping_response<'a>(revision: u64, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut map = Map::with_capacity_in(3, alloc);
    insert_bool(&mut map, "ok", true, alloc);
    insert_u64(&mut map, "protocol", u64::from(PROTOCOL_VERSION), alloc);
    insert_u64(&mut map, "revision", revision, alloc);
    Value::Map(map)
}

fn build_info_response<'a>(
    reg: &TermRegistry<'a>,
    meta: ServerMeta<'_>,
    alloc: Alloc<'a>,
) -> Value<Alloc<'a>> {
    let mut map = Map::with_capacity_in(8, alloc);
    insert_bool(&mut map, "ok", true, alloc);
    insert_u64(&mut map, "protocol", u64::from(PROTOCOL_VERSION), alloc);
    insert_u64(&mut map, "pid", u64::from(meta.pid), alloc);
    insert_str(&mut map, "socket", meta.socket_path, alloc);
    insert_u64(&mut map, "revision", reg.revision(), alloc);
    insert_u64(&mut map, "symbols", u64::try_from(reg.symbols().len()).unwrap_or(u64::MAX), alloc);
    insert_u64(
        &mut map,
        "entities",
        u64::try_from(reg.entities().len()).unwrap_or(u64::MAX),
        alloc,
    );

    let mut range = Vec::with_capacity_in(2, alloc);
    range.push(Value::Text(blaze_own::format_in!(alloc, "U+{PUA_START:04X}")));
    range.push(Value::Text(blaze_own::format_in!(alloc, "U+{PUA_END:04X}")));
    map.insert(String::from_str_in("pua_range", alloc), Value::List(range));

    Value::Map(map)
}

fn handle_symbol_register<'a>(
    reg: &mut TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let name = req_str(map, "name", alloc)?;
    let requested_cp = opt_codepoint(map, "codepoint", alloc)?;
    let width = match opt_u64(map, "width", alloc)? {
        Some(1) | None => 1u8,
        Some(2) => 2u8,
        Some(other) => {
            return Err(ApiError::InvalidField {
                field: "width",
                reason: blaze_own::format_in!(alloc, "expected 1 or 2, got {other}"),
            });
        }
    };
    let source = parse_symbol_source(map, alloc)?;
    let record = reg.register_symbol(name, requested_cp, width, source)?;

    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    out.insert(String::from_str_in("symbol", alloc), symbol_to_value(record, alloc));
    Ok((Value::Map(out), true))
}

fn handle_symbol_unregister<'a>(
    reg: &mut TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let name = req_str(map, "name", alloc)?;
    let removed = reg.unregister_symbol(name);
    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    insert_bool(&mut out, "removed", removed, alloc);
    Ok((Value::Map(out), removed))
}

fn handle_symbol_get<'a>(
    reg: &TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let name = opt_str(map, "name", alloc)?;
    let cp = opt_codepoint(map, "codepoint", alloc)?;

    let record = match (name, cp) {
        (Some(n), _) => reg.symbol_by_name(n).ok_or_else(|| ApiError::NotFound {
            kind: "symbol",
            key: String::from_str_in(n, alloc),
        })?,
        (None, Some(c)) => reg.symbol_by_codepoint(c).ok_or_else(|| ApiError::NotFound {
            kind: "symbol",
            key: blaze_own::format_in!(alloc, "U+{c:04X}"),
        })?,
        (None, None) => {
            return Err(ApiError::InvalidField {
                field: "name",
                reason: String::from_str_in("provide `name` or `codepoint`", alloc),
            });
        }
    };

    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    out.insert(String::from_str_in("symbol", alloc), symbol_to_value(record, alloc));
    Ok((Value::Map(out), false))
}

fn build_symbol_list_response<'a>(
    reg: &TermRegistry<'a>,
    alloc: Alloc<'a>,
) -> Value<Alloc<'a>> {
    let mut out = Map::with_capacity_in(3, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    insert_u64(&mut out, "revision", reg.revision(), alloc);
    out.insert(String::from_str_in("symbols", alloc), sorted_symbols_list(reg.symbols(), alloc));
    Value::Map(out)
}

fn handle_entity_bind<'a>(
    reg: &mut TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let id = parse_entity_id_from_map(map, alloc)?;
    let symbol = opt_str(map, "symbol", alloc)?.map(|s| String::from_str_in(s, alloc));
    let display_name = opt_str(map, "display_name", alloc)?.map(|s| String::from_str_in(s, alloc));
    let color = opt_hex_color(map, "color", alloc)?;
    let badge = opt_str(map, "badge", alloc)?.map(|s| String::from_str_in(s, alloc));
    let hint_path = opt_str(map, "hint_path", alloc)?.map(|s| String::from_str_in(s, alloc));
    let attrs = parse_attrs_map(map, alloc)?;

    let meta = EntityMeta {
        id,
        symbol,
        display_name,
        color,
        badge,
        hint_path,
        attrs,
        revision: 0,
    };
    let stored = reg.bind_entity(meta)?;

    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    out.insert(String::from_str_in("entity", alloc), entity_to_value(stored, alloc));
    Ok((Value::Map(out), true))
}

fn handle_entity_unbind<'a>(
    reg: &mut TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let id = parse_entity_id_from_map(map, alloc)?;
    let removed = reg.unbind_entity(id.kind, id.id.as_str());
    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    insert_bool(&mut out, "removed", removed, alloc);
    Ok((Value::Map(out), removed))
}

fn handle_entity_get<'a>(
    reg: &TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let id = parse_entity_id_from_map(map, alloc)?;
    let found = reg.entity(id.kind, id.id.as_str()).ok_or_else(|| ApiError::NotFound {
        kind: "entity",
        key: blaze_own::format_in!(alloc, "{}:{}", id.kind.as_str(), id.id.as_str()),
    })?;
    let mut out = Map::with_capacity_in(2, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    out.insert(String::from_str_in("entity", alloc), entity_to_value(found, alloc));
    Ok((Value::Map(out), false))
}

fn handle_entity_list<'a>(
    reg: &TermRegistry<'a>,
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<(Value<Alloc<'a>>, bool), ApiError<'a>> {
    let filter_kind = match opt_str(map, "kind", alloc)? {
        Some(k) => Some(EntityKind::from_str(k).ok_or_else(|| ApiError::InvalidField {
            field: "kind",
            reason: blaze_own::format_in!(alloc, "unknown entity kind `{k}`"),
        })?),
        None => None,
    };

    let mut out = Map::with_capacity_in(3, alloc);
    insert_bool(&mut out, "ok", true, alloc);
    insert_u64(&mut out, "revision", reg.revision(), alloc);
    out.insert(
        String::from_str_in("entities", alloc),
        sorted_entities_list(reg.entities(), filter_kind, alloc),
    );
    Ok((Value::Map(out), false))
}

pub(crate) fn encode_snapshot<'a>(reg: &TermRegistry<'a>, alloc: Alloc<'a>) -> AString<'a> {
    let mut map = Map::with_capacity_in(4, alloc);
    insert_u64(&mut map, "version", u64::from(PROTOCOL_VERSION), alloc);
    insert_u64(&mut map, "revision", reg.revision(), alloc);
    map.insert(String::from_str_in("symbols", alloc), sorted_symbols_list(reg.symbols(), alloc));
    map.insert(
        String::from_str_in("entities", alloc),
        sorted_entities_list(reg.entities(), None, alloc),
    );
    encode_json(&Value::Map(map), alloc)
}

pub(crate) fn decode_snapshot<'a>(
    raw: &[u8],
    alloc: Alloc<'a>,
) -> Result<TermRegistry<'a>, ApiError<'a>> {
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError::TooLarge { len: raw.len(), max: MAX_MESSAGE_BYTES });
    }
    let val = read_in(raw, json_limits(), alloc)
        .map_err(|e| ApiError::InvalidJson(blaze_own::format_in!(alloc, "{e}")))?;
    let Some(root) = val.as_map() else {
        return Err(ApiError::InvalidJson(String::from_str_in(
            "snapshot root must be an object",
            alloc,
        )));
    };

    let version = req_u64(root, "version", alloc)?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(ApiError::InvalidField {
            field: "version",
            reason: blaze_own::format_in!(
                alloc,
                "unsupported snapshot version {version} (expected {PROTOCOL_VERSION})"
            ),
        });
    }
    let revision = req_u64(root, "revision", alloc)?;

    let Some(symbols_val) = root.get("symbols").and_then(Value::as_list) else {
        return Err(ApiError::InvalidField {
            field: "symbols",
            reason: String::from_str_in("expected array of symbol objects", alloc),
        });
    };
    if symbols_val.len() > MAX_SYMBOLS {
        return Err(ApiError::CapacityExceeded { kind: "symbols", max: MAX_SYMBOLS });
    }

    let mut symbols: AVec<'a, SymbolRecord<'a>> = Vec::with_capacity_in(symbols_val.len(), alloc);
    for item in symbols_val.iter() {
        let Some(sym_map) = item.as_map() else {
            return Err(ApiError::InvalidField {
                field: "symbols",
                reason: String::from_str_in("every symbol entry must be an object", alloc),
            });
        };
        let name = req_str(sym_map, "name", alloc)?;
        if !is_valid_symbol_name(name) {
            return Err(ApiError::InvalidField {
                field: "name",
                reason: String::from_str_in(
                    "must be 1..=96 bytes matching [a-zA-Z0-9._:-]",
                    alloc,
                ),
            });
        }
        if symbols.iter().any(|s| s.name.as_str() == name) {
            return Err(ApiError::InvalidField {
                field: "symbols",
                reason: blaze_own::format_in!(alloc, "duplicate symbol name `{name}`"),
            });
        }
        let codepoint = req_codepoint(sym_map, "codepoint", alloc)?;
        if !(PUA_START..=PUA_END).contains(&codepoint) {
            return Err(ApiError::CodepointOutOfRange { codepoint });
        }
        if let Some(owner) = symbols.iter().find(|s| s.codepoint == codepoint) {
            return Err(ApiError::CodepointInUse {
                codepoint,
                owner: String::from_str_in(owner.name.as_str(), alloc),
            });
        }
        let width_cells = match req_u64(sym_map, "width", alloc)? {
            1 => 1u8,
            2 => 2u8,
            other => {
                return Err(ApiError::InvalidField {
                    field: "width",
                    reason: blaze_own::format_in!(alloc, "expected 1 or 2, got {other}"),
                });
            }
        };
        let source = parse_symbol_source(sym_map, alloc)?;
        validate_symbol_source(&source, alloc)?;
        let sym_rev = req_u64(sym_map, "revision", alloc)?;
        symbols.push(SymbolRecord {
            name: String::from_str_in(name, alloc),
            codepoint,
            width_cells,
            source,
            revision: sym_rev,
        });
    }

    let Some(entities_val) = root.get("entities").and_then(Value::as_list) else {
        return Err(ApiError::InvalidField {
            field: "entities",
            reason: String::from_str_in("expected array of entity objects", alloc),
        });
    };
    if entities_val.len() > MAX_ENTITIES {
        return Err(ApiError::CapacityExceeded { kind: "entities", max: MAX_ENTITIES });
    }

    let mut entities: AVec<'a, EntityMeta<'a>> = Vec::with_capacity_in(entities_val.len(), alloc);
    for item in entities_val.iter() {
        let Some(ent_map) = item.as_map() else {
            return Err(ApiError::InvalidField {
                field: "entities",
                reason: String::from_str_in("every entity entry must be an object", alloc),
            });
        };
        let id = parse_entity_id_from_map(ent_map, alloc)?;
        if entities
            .iter()
            .any(|e| e.id.kind == id.kind && e.id.id.as_str() == id.id.as_str())
        {
            return Err(ApiError::InvalidField {
                field: "entities",
                reason: blaze_own::format_in!(
                    alloc,
                    "duplicate entity `{}:{}`",
                    id.kind.as_str(),
                    id.id.as_str()
                ),
            });
        }
        let symbol = opt_str(ent_map, "symbol", alloc)?.map(|s| String::from_str_in(s, alloc));
        let display_name =
            opt_str(ent_map, "display_name", alloc)?.map(|s| String::from_str_in(s, alloc));
        let color = opt_hex_color(ent_map, "color", alloc)?;
        let badge = opt_str(ent_map, "badge", alloc)?.map(|s| String::from_str_in(s, alloc));
        let hint_path =
            opt_str(ent_map, "hint_path", alloc)?.map(|s| String::from_str_in(s, alloc));
        let mut attrs = parse_attrs_map(ent_map, alloc)?;
        normalize_attrs(&mut attrs);
        let ent_rev = req_u64(ent_map, "revision", alloc)?;
        entities.push(EntityMeta {
            id,
            symbol,
            display_name,
            color,
            badge,
            hint_path,
            attrs,
            revision: ent_rev,
        });
    }

    Ok(TermRegistry::from_validated_parts(alloc, revision, symbols, entities))
}

fn sorted_symbols_list<'a>(symbols: &[SymbolRecord<'a>], alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut indices: AVec<'a, usize> = Vec::with_capacity_in(symbols.len(), alloc);
    for i in 0..symbols.len() {
        indices.push(i);
    }
    indices.sort_unstable_by(|&a, &b| symbols[a].name.as_str().cmp(symbols[b].name.as_str()));

    let mut list = Vec::with_capacity_in(indices.len(), alloc);
    for idx in indices.iter().copied() {
        list.push(symbol_to_value(&symbols[idx], alloc));
    }
    Value::List(list)
}

fn sorted_entities_list<'a>(
    entities: &[EntityMeta<'a>],
    filter_kind: Option<EntityKind>,
    alloc: Alloc<'a>,
) -> Value<Alloc<'a>> {
    let mut indices: AVec<'a, usize> = Vec::with_capacity_in(entities.len(), alloc);
    for (i, ent) in entities.iter().enumerate() {
        if filter_kind.is_none_or(|k| ent.id.kind == k) {
            indices.push(i);
        }
    }
    indices.sort_unstable_by(|&a, &b| {
        let ea = &entities[a];
        let eb = &entities[b];
        ea.id
            .kind
            .as_str()
            .cmp(eb.id.kind.as_str())
            .then_with(|| ea.id.id.as_str().cmp(eb.id.id.as_str()))
    });

    let mut list = Vec::with_capacity_in(indices.len(), alloc);
    for idx in indices.iter().copied() {
        list.push(entity_to_value(&entities[idx], alloc));
    }
    Value::List(list)
}

fn symbol_to_value<'a>(record: &SymbolRecord<'a>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut map = Map::with_capacity_in(12, alloc);
    insert_str(&mut map, "name", record.name.as_str(), alloc);
    insert_u64(&mut map, "codepoint", u64::from(record.codepoint), alloc);
    let cp_hex = blaze_own::format_in!(alloc, "U+{:04X}", record.codepoint);
    map.insert(String::from_str_in("codepoint_hex", alloc), Value::Text(cp_hex));

    let mut utf8 = String::with_capacity_in(4, alloc);
    if let Some(ch) = char::from_u32(record.codepoint) {
        utf8.push(ch);
    }
    map.insert(String::from_str_in("utf8", alloc), Value::Text(utf8));

    insert_u64(&mut map, "width", u64::from(record.width_cells), alloc);
    insert_str(&mut map, "kind", record.source.kind_str(), alloc);

    match &record.source {
        SymbolSource::Svg { markup, monochrome } => {
            insert_str(&mut map, "svg", markup.as_str(), alloc);
            insert_bool(&mut map, "monochrome", *monochrome, alloc);
        }
        SymbolSource::FontGlyph { font_name, font_data_b64, codepoint, tint } => {
            if let Some(name) = font_name {
                insert_str(&mut map, "font_name", name.as_str(), alloc);
            }
            if let Some(b64) = font_data_b64 {
                insert_str(&mut map, "font_data_b64", b64.as_str(), alloc);
            }
            insert_u64(&mut map, "glyph_codepoint", u64::from(*codepoint), alloc);
            if let Some(rgba) = tint {
                map.insert(
                    String::from_str_in("tint", alloc),
                    Value::Text(format_hex_color(*rgba, alloc)),
                );
            }
        }
        SymbolSource::Png { data_b64 } => {
            insert_str(&mut map, "data_b64", data_b64.as_str(), alloc);
        }
        SymbolSource::Text { text } => {
            insert_str(&mut map, "text", text.as_str(), alloc);
        }
    }

    insert_u64(&mut map, "revision", record.revision, alloc);
    Value::Map(map)
}

fn entity_to_value<'a>(meta: &EntityMeta<'a>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut map = Map::with_capacity_in(11, alloc);
    insert_str(&mut map, "kind", meta.id.kind.as_str(), alloc);
    insert_str(&mut map, "id", meta.id.id.as_str(), alloc);
    if let Some((dev, ino)) = meta.id.parse_file_dev_ino() {
        insert_u64(&mut map, "dev", dev, alloc);
        insert_u64(&mut map, "ino", ino, alloc);
    }
    if let Some(sym) = &meta.symbol {
        insert_str(&mut map, "symbol", sym.as_str(), alloc);
    }
    if let Some(dn) = &meta.display_name {
        insert_str(&mut map, "display_name", dn.as_str(), alloc);
    }
    if let Some(rgba) = meta.color {
        map.insert(
            String::from_str_in("color", alloc),
            Value::Text(format_hex_color(rgba, alloc)),
        );
    }
    if let Some(badge) = &meta.badge {
        insert_str(&mut map, "badge", badge.as_str(), alloc);
    }
    if let Some(hint) = &meta.hint_path {
        insert_str(&mut map, "hint_path", hint.as_str(), alloc);
    }

    let mut attr_indices: AVec<'a, usize> = Vec::with_capacity_in(meta.attrs.len(), alloc);
    for i in 0..meta.attrs.len() {
        attr_indices.push(i);
    }
    attr_indices.sort_unstable_by(|&a, &b| {
        meta.attrs[a]
            .0
            .as_str()
            .cmp(meta.attrs[b].0.as_str())
            .then_with(|| a.cmp(&b))
    });

    let mut attrs_map = Map::with_capacity_in(attr_indices.len(), alloc);
    for idx in attr_indices.iter().copied() {
        let (k, v) = &meta.attrs[idx];
        insert_str(&mut attrs_map, k.as_str(), v.as_str(), alloc);
    }
    map.insert(String::from_str_in("attrs", alloc), Value::Map(attrs_map));
    insert_u64(&mut map, "revision", meta.revision, alloc);
    Value::Map(map)
}

fn parse_symbol_source<'a>(
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<SymbolSource<'a>, ApiError<'a>> {
    let kind = req_str(map, "kind", alloc)?;
    match kind {
        "svg" => {
            let markup = req_str(map, "svg", alloc)?;
            let monochrome = opt_bool(map, "monochrome", alloc)?.unwrap_or(false);
            Ok(SymbolSource::Svg { markup: String::from_str_in(markup, alloc), monochrome })
        }
        "font_glyph" => {
            let font_name = opt_str(map, "font_name", alloc)?.map(|s| String::from_str_in(s, alloc));
            let font_data_b64 =
                opt_str(map, "font_data_b64", alloc)?.map(|s| String::from_str_in(s, alloc));
            let codepoint = req_codepoint(map, "glyph_codepoint", alloc)?;
            let tint = opt_hex_color(map, "tint", alloc)?;
            Ok(SymbolSource::FontGlyph { font_name, font_data_b64, codepoint, tint })
        }
        "png" => {
            let data_b64 = req_str(map, "data_b64", alloc)?;
            Ok(SymbolSource::Png { data_b64: String::from_str_in(data_b64, alloc) })
        }
        "text" => {
            let text = req_str(map, "text", alloc)?;
            Ok(SymbolSource::Text { text: String::from_str_in(text, alloc) })
        }
        other => Err(ApiError::InvalidField {
            field: "kind",
            reason: blaze_own::format_in!(
                alloc,
                "unknown symbol kind `{other}` (expected svg, font_glyph, png, or text)"
            ),
        }),
    }
}

fn parse_entity_id_from_map<'a>(
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<EntityId<'a>, ApiError<'a>> {
    let kind_s = req_str(map, "kind", alloc)?;
    let kind = EntityKind::from_str(kind_s).ok_or_else(|| ApiError::InvalidField {
        field: "kind",
        reason: blaze_own::format_in!(alloc, "unknown entity kind `{kind_s}`"),
    })?;
    let id = opt_str(map, "id", alloc)?;
    let dev = opt_u64(map, "dev", alloc)?;
    let ino = opt_u64(map, "ino", alloc)?;
    resolve_entity_id(kind, id, dev, ino, alloc)
}

fn parse_attrs_map<'a>(
    map: &Map<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, (AString<'a>, AString<'a>)>, ApiError<'a>> {
    let Some(val) = map.get("attrs") else {
        return Ok(Vec::new_in(alloc));
    };
    if val.is_null() {
        return Ok(Vec::new_in(alloc));
    }
    let Some(attrs_map) = val.as_map() else {
        return Err(ApiError::InvalidField {
            field: "attrs",
            reason: String::from_str_in("expected object mapping string keys to string values", alloc),
        });
    };
    let mut out = Vec::with_capacity_in(attrs_map.len(), alloc);
    for (k, v) in attrs_map.sorted() {
        let Some(s) = v.as_str() else {
            return Err(ApiError::InvalidField {
                field: "attrs",
                reason: blaze_own::format_in!(alloc, "attribute `{k}` must be a string"),
            });
        };
        out.push((String::from_str_in(k, alloc), String::from_str_in(s, alloc)));
    }
    Ok(out)
}

/// Parse compact `;`-separated `k=v` OSC 7770 shorthand into the same `Map` form as JSON-RPC.
fn parse_compact_osc_kv<'a>(
    raw: &[u8],
    alloc: Alloc<'a>,
) -> Result<(Map<Alloc<'a>>, bool), ApiError<'a>> {
    let text = core::str::from_utf8(raw).map_err(|_| ApiError::InvalidField {
        field: "payload",
        reason: String::from_str_in("OSC 7770 payload must be valid UTF-8", alloc),
    })?;

    let mut pairs: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    let mut op_value: Option<&str> = None;
    for part in text.split(';') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            return Err(ApiError::InvalidField {
                field: "payload",
                reason: blaze_own::format_in!(alloc, "expected `key=value` segment, got `{trimmed}`"),
            });
        };
        let key = k.trim();
        if key.is_empty() {
            return Err(ApiError::InvalidField {
                field: "payload",
                reason: String::from_str_in("empty key in `key=value` segment", alloc),
            });
        }
        if key == "op" {
            op_value = Some(v);
        }
        pairs.push((key, v));
    }

    let mut map = Map::with_capacity_in(pairs.len() + 2, alloc);
    let mut attrs = Map::new_in(alloc);
    let mut want_reply = false;
    let is_entity_bind = op_value == Some("entity.bind");
    let is_symbol_register = op_value == Some("symbol.register");

    for &(key, val) in pairs.iter() {
        if let Some(attr_key) = key.strip_prefix("attr.") {
            if attr_key.is_empty() {
                return Err(ApiError::InvalidField {
                    field: "attrs",
                    reason: String::from_str_in("empty attribute key after `attr.`", alloc),
                });
            }
            insert_str(&mut attrs, attr_key, val, alloc);
            continue;
        }
        match key {
            "reply" => {
                want_reply = parse_kv_bool(val, "reply", alloc)?;
            }
            "monochrome" => {
                let b = parse_kv_bool(val, "monochrome", alloc)?;
                insert_bool(&mut map, "monochrome", b, alloc);
            }
            "width" => {
                let n = parse_strict_u64(val).ok_or_else(|| ApiError::InvalidField {
                    field: "width",
                    reason: blaze_own::format_in!(alloc, "expected integer, got `{val}`"),
                })?;
                insert_u64(&mut map, "width", n, alloc);
            }
            "dev" => {
                let n = parse_strict_u64(val).ok_or_else(|| ApiError::InvalidField {
                    field: "dev",
                    reason: blaze_own::format_in!(alloc, "expected unsigned integer, got `{val}`"),
                })?;
                insert_u64(&mut map, "dev", n, alloc);
            }
            "ino" => {
                let n = parse_strict_u64(val).ok_or_else(|| ApiError::InvalidField {
                    field: "ino",
                    reason: blaze_own::format_in!(alloc, "expected unsigned integer, got `{val}`"),
                })?;
                insert_u64(&mut map, "ino", n, alloc);
            }
            "cp" | "codepoint" => {
                insert_str(&mut map, "codepoint", val, alloc);
            }
            "png" => {
                insert_str(&mut map, "data_b64", val, alloc);
            }
            "name" if is_entity_bind => {
                insert_str(&mut map, "display_name", val, alloc);
            }
            other => {
                insert_str(&mut map, other, val, alloc);
            }
        }
    }

    if !attrs.is_empty() {
        map.insert(String::from_str_in("attrs", alloc), Value::Map(attrs));
    }

    if is_symbol_register && !map.contains_key("kind") {
        let inferred = if map.contains_key("text") {
            Some("text")
        } else if map.contains_key("svg") {
            Some("svg")
        } else if map.contains_key("data_b64") {
            Some("png")
        } else if map.contains_key("glyph_codepoint")
            || map.contains_key("font_name")
            || map.contains_key("font_data_b64")
        {
            Some("font_glyph")
        } else {
            None
        };
        if let Some(kind) = inferred {
            insert_str(&mut map, "kind", kind, alloc);
        }
    }

    Ok((map, want_reply))
}

fn parse_kv_bool<'a>(s: &str, field: &'static str, alloc: Alloc<'a>) -> Result<bool, ApiError<'a>> {
    match s {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected boolean (`true`/`false`/`1`/`0`), got `{other}`"),
        }),
    }
}

fn req_str<'a, 'm>(
    map: &'m Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<&'m str, ApiError<'a>> {
    match map.get(field) {
        Some(v) => v.as_str().ok_or_else(|| ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected string, found {}", v.kind()),
        }),
        None => Err(ApiError::InvalidField {
            field,
            reason: String::from_str_in("missing required field", alloc),
        }),
    }
}

fn opt_str<'a, 'm>(
    map: &'m Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<Option<&'m str>, ApiError<'a>> {
    match map.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(|| ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected string, found {}", v.kind()),
        }),
    }
}

fn opt_bool<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<Option<bool>, ApiError<'a>> {
    match map.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected boolean, found {}", v.kind()),
        }),
    }
}

fn req_u64<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<u64, ApiError<'a>> {
    match map.get(field) {
        Some(v) => v.as_u64().ok_or_else(|| ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected unsigned integer, found {}", v.kind()),
        }),
        None => Err(ApiError::InvalidField {
            field,
            reason: String::from_str_in("missing required field", alloc),
        }),
    }
}

fn opt_u64<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<Option<u64>, ApiError<'a>> {
    match map.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(alloc, "expected unsigned integer, found {}", v.kind()),
        }),
    }
}

fn req_codepoint<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<u32, ApiError<'a>> {
    match opt_codepoint(map, field, alloc)? {
        Some(cp) => Ok(cp),
        None => Err(ApiError::InvalidField {
            field,
            reason: String::from_str_in("missing required codepoint", alloc),
        }),
    }
}

fn opt_codepoint<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<Option<u32>, ApiError<'a>> {
    match map.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(Value::Int(n)) => {
            let cp = n
                .as_u64()
                .and_then(|u| u32::try_from(u).ok())
                .ok_or_else(|| ApiError::InvalidField {
                    field,
                    reason: String::from_str_in("codepoint integer out of u32 range", alloc),
                })?;
            Ok(Some(cp))
        }
        Some(Value::Text(s)) => {
            let cp = parse_codepoint_str(s.as_str()).ok_or_else(|| ApiError::InvalidField {
                field,
                reason: blaze_own::format_in!(
                    alloc,
                    "invalid codepoint `{}` (expected integer, U+XXXX, or 0xXXXX)",
                    s.as_str()
                ),
            })?;
            Ok(Some(cp))
        }
        Some(other) => Err(ApiError::InvalidField {
            field,
            reason: blaze_own::format_in!(
                alloc,
                "expected integer or string codepoint, found {}",
                other.kind()
            ),
        }),
    }
}

fn opt_hex_color<'a>(
    map: &Map<Alloc<'a>>,
    field: &'static str,
    alloc: Alloc<'a>,
) -> Result<Option<[u8; 4]>, ApiError<'a>> {
    let Some(raw) = opt_str(map, field, alloc)? else {
        return Ok(None);
    };
    let rgba = parse_hex_color(raw).ok_or_else(|| ApiError::InvalidField {
        field,
        reason: blaze_own::format_in!(
            alloc,
            "invalid hex color `{raw}` (expected #rrggbb or #rrggbbaa)"
        ),
    })?;
    Ok(Some(rgba))
}

fn insert_str<'a>(map: &mut Map<Alloc<'a>>, key: &str, val: &str, alloc: Alloc<'a>) {
    map.insert(
        String::from_str_in(key, alloc),
        Value::Text(String::from_str_in(val, alloc)),
    );
}

fn insert_bool<'a>(map: &mut Map<Alloc<'a>>, key: &str, val: bool, alloc: Alloc<'a>) {
    map.insert(String::from_str_in(key, alloc), Value::Bool(val));
}

fn insert_u64<'a>(map: &mut Map<Alloc<'a>>, key: &str, val: u64, alloc: Alloc<'a>) {
    map.insert(String::from_str_in(key, alloc), Value::Int(Int::from_u64(val)));
}

fn trim_ascii_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}
