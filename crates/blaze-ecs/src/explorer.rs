//! Pure sans-IO Flecs Explorer HTTP/JSON/TAB API and embedded interactive Web UI
//! (`#![no_std]`, `#![forbid(unsafe_code)]`, zero sockets, zero ambient I/O).
//!
//! Callers supply raw HTTP/1.1 bytes via [`handle_http_request`] (or pre-parsed `(method, path,
//! body)` via [`handle_request`]) and receive deterministic response bytes backed by their
//! explicit allocator `A`.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_own::{AllocFailure, Erased, String as OwnString, Vec as OwnVec};

use blaze_data::{Int, Map, Value};
use blaze_json::{Limits as JsonLimits, WriteOptions as JsonWriteOptions};

use super::error::EcsError;
use super::id::{Entity, Id};
use super::query::QuerySpec;
use super::reflect::ReflectError;
use super::world::World;

/// Structured HTTP response produced by [`handle_request`].
pub struct ExplorerResponse<A: Allocator> {
    /// HTTP status code (e.g. `200`, `201`, `204`, `400`, `404`).
    pub status: u16,
    /// Static `Content-Type` header value.
    pub content_type: &'static str,
    /// Response body bytes allocated in the caller's allocator.
    pub body: OwnVec<u8, A>,
}

const CT_HTML: &str = "text/html; charset=utf-8";
const CT_JSON: &str = "application/json";
const CT_TEXT: &str = "text/plain; charset=utf-8";
const CT_WASM: &str = "application/wasm";

/// Handle a single Explorer API or UI request against `world` with zero ambient I/O.
pub fn handle_request<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    method: &str,
    path: &str,
    body: &[u8],
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    handle_request_with_wasm(world, method, path, body, None, alloc)
}

/// Handle a single Explorer API, UI, or `/explorer.wasm` request against `world` with zero ambient
/// I/O.
pub fn handle_request_with_wasm<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    method: &str,
    path: &str,
    body: &[u8],
    wasm: Option<&[u8]>,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    if method == "OPTIONS" {
        return Ok(ExplorerResponse {
            status: 204,
            content_type: CT_TEXT,
            body: OwnVec::new_in(alloc),
        });
    }

    let (route_path, query_str) = split_path_query(path);

    if method == "GET" && (route_path == "/" || route_path == "/index.html") {
        let mut out = OwnVec::new_in(alloc);
        push_bytes(&mut out, EXPLORER_HTML.as_bytes())?;
        return Ok(ExplorerResponse { status: 200, content_type: CT_HTML, body: out });
    }

    if method == "GET" && route_path == "/explorer.wasm" {
        return match wasm {
            Some(bytes) => {
                let mut out = OwnVec::new_in(alloc);
                push_bytes(&mut out, bytes)?;
                Ok(ExplorerResponse { status: 200, content_type: CT_WASM, body: out })
            }
            None => json_error(404, "explorer.wasm not attached", alloc),
        };
    }

    if method == "GET" && route_path == "/api/world" {
        return match world.world_to_value(alloc) {
            Ok(val) => json_value_response(200, &val, alloc),
            Err(err) => reflect_error_response(err, alloc),
        };
    }

    if method == "GET" && route_path == "/api/world.tab" {
        let val = match world.world_to_value(alloc) {
            Ok(v) => v,
            Err(err) => return reflect_error_response(err, alloc),
        };
        let erased = Erased::new(alloc);
        let tab_result = blaze_tab::write_pretty(&val, erased.handle());
        let mut out = OwnVec::new_in(alloc);
        match tab_result {
            Ok(text) => {
                push_bytes(&mut out, text.as_str().as_bytes())?;
                return Ok(ExplorerResponse { status: 200, content_type: CT_TEXT, body: out });
            }
            Err(_) => {
                return json_error(500, "failed to serialize TAB snapshot", alloc);
            }
        }
    }

    if method == "GET" && route_path == "/api/query" {
        return handle_query(world, query_str, alloc);
    }

    if method == "POST" && route_path == "/api/entity" {
        return handle_post_entity(world, body, alloc);
    }

    if let Some(rest) = route_path.strip_prefix("/api/entity/") {
        return handle_entity_subroute(world, method, rest, body, alloc);
    }

    json_error(404, "not found", alloc)
}

/// Parse a raw HTTP/1.1 request, dispatch it through [`handle_request`], and serialize a full
/// HTTP/1.1 response with CORS and `Content-Length` headers.
pub fn handle_http_request<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    raw_http: &[u8],
    alloc: A,
) -> Result<OwnVec<u8, A>, AllocFailure> {
    let response = match parse_http_request(raw_http) {
        Some((method, target, req_body)) => handle_request(world, method, target, req_body, alloc)?,
        None => json_error(400, "malformed HTTP/1.1 request", alloc)?,
    };

    let mut head = OwnString::new_in(alloc);
    let reason = status_reason(response.status);
    let _ = write!(
        head,
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
    );

    let total_len = head.len().saturating_add(response.body.len());
    let mut wire = OwnVec::try_with_capacity_in(total_len, alloc)?;
    push_bytes(&mut wire, head.as_str().as_bytes())?;
    push_bytes(&mut wire, response.body.as_slice())?;
    Ok(wire)
}

// -----------------------------------------------------------------------------
// Route Handlers
// -----------------------------------------------------------------------------

fn handle_entity_subroute<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    method: &str,
    rest: &str,
    body: &[u8],
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let mut parts = rest.split('/');
    let Some(slot_str) = parts.next() else {
        return json_error(404, "not found", alloc);
    };
    let Some(seg1) = parts.next() else {
        // `/api/entity/<slot>`
        let Ok(slot) = slot_str.parse::<u32>() else {
            return json_error(400, "invalid entity slot", alloc);
        };
        let Some(entity) = world.entity_by_slot(slot) else {
            return json_error(404, "entity not found", alloc);
        };
        if method == "GET" {
            return entity_json_response(world, 200, entity, alloc);
        }
        if method == "DELETE" {
            return match world.despawn(entity) {
                Ok(()) => {
                    let mut map = Map::new_in(alloc);
                    let _ = map.insert(
                        OwnString::try_from_str_in("deleted", alloc)?,
                        Value::Int(Int::from_u64(u64::from(slot))),
                    );
                    json_value_response(200, &Value::Map(map), alloc)
                }
                Err(EcsError::DeadEntity(_)) => json_error(404, "entity not found", alloc),
                Err(EcsError::Alloc(a)) => Err(a),
                Err(_) => json_error(400, "failed to despawn entity", alloc),
            };
        }
        return json_error(404, "not found", alloc);
    };

    let Ok(slot) = slot_str.parse::<u32>() else {
        return json_error(400, "invalid entity slot", alloc);
    };

    match seg1 {
        "component" => {
            let Some(raw_name) = parts.next() else {
                return json_error(404, "not found", alloc);
            };
            if parts.next().is_some() || raw_name.is_empty() {
                return json_error(404, "not found", alloc);
            }
            if method != "PATCH" && method != "PUT" {
                return json_error(404, "not found", alloc);
            }
            let Some(entity) = world.entity_by_slot(slot) else {
                return json_error(404, "entity not found", alloc);
            };
            let comp_name = percent_decode(raw_name, alloc)?;
            let patch_val = match blaze_json::read_in(body, JsonLimits::SHIPPED, alloc) {
                Ok(v) => v,
                Err(_) => return json_error(400, "invalid JSON body", alloc),
            };
            let result = if method == "PATCH" {
                world.patch_component_by_name(entity, comp_name.as_str(), &patch_val, alloc)
            } else {
                world.set_component_from_value(entity, comp_name.as_str(), &patch_val, alloc)
            };
            match result {
                Ok(()) => entity_json_response(world, 200, entity, alloc),
                Err(err) => reflect_error_response(err, alloc),
            }
        }
        "tag" => {
            let Some(raw_tag) = parts.next() else {
                return json_error(404, "not found", alloc);
            };
            if parts.next().is_some() || raw_tag.is_empty() {
                return json_error(404, "not found", alloc);
            }
            let Some(entity) = world.entity_by_slot(slot) else {
                return json_error(404, "entity not found", alloc);
            };
            let tag_name = percent_decode(raw_tag, alloc)?;
            if tag_name.is_empty() {
                return json_error(400, "tag name cannot be empty", alloc);
            }
            if method == "POST" {
                let tag_ent = match world.entity_by_name(tag_name.as_str()) {
                    Some(existing) => existing,
                    None => match world.spawn_named(tag_name.as_str()) {
                        Ok(created) => created,
                        Err(EcsError::Alloc(a)) => return Err(a),
                        Err(_) => return json_error(400, "failed to create tag entity", alloc),
                    },
                };
                return match world.add_tag(entity, tag_ent) {
                    Ok(()) => entity_json_response(world, 200, entity, alloc),
                    Err(EcsError::Alloc(a)) => Err(a),
                    Err(EcsError::DeadEntity(_)) => json_error(404, "entity not found", alloc),
                    Err(_) => json_error(400, "failed to add tag", alloc),
                };
            }
            if method == "DELETE" {
                if let Some(tag_ent) = world.entity_by_name(tag_name.as_str()) {
                    match world.remove_tag(entity, tag_ent) {
                        Ok(_) => {}
                        Err(EcsError::Alloc(a)) => return Err(a),
                        Err(EcsError::DeadEntity(_)) => {
                            return json_error(404, "entity not found", alloc);
                        }
                        Err(_) => return json_error(400, "failed to remove tag", alloc),
                    }
                }
                return entity_json_response(world, 200, entity, alloc);
            }
            json_error(404, "not found", alloc)
        }
        "parent" => {
            if method == "DELETE" && parts.next().is_none() {
                let Some(entity) = world.entity_by_slot(slot) else {
                    return json_error(404, "entity not found", alloc);
                };
                let existing = match world.parent_of(entity) {
                    Ok(p) => p,
                    Err(EcsError::Alloc(a)) => return Err(a),
                    Err(EcsError::DeadEntity(_)) => {
                        return json_error(404, "entity not found", alloc);
                    }
                    Err(_) => return json_error(400, "failed to query parent", alloc),
                };
                if let Some(p) = existing {
                    let rel = world.child_of();
                    if let Err(err) = world.remove_pair(entity, rel, p) {
                        return ecs_error_response(err, alloc);
                    }
                }
                return entity_json_response(world, 200, entity, alloc);
            }
            if method == "POST" {
                let Some(parent_str) = parts.next() else {
                    return json_error(404, "not found", alloc);
                };
                if parts.next().is_some() {
                    return json_error(404, "not found", alloc);
                }
                let Ok(parent_slot) = parent_str.parse::<u32>() else {
                    return json_error(400, "invalid parent slot", alloc);
                };
                let Some(entity) = world.entity_by_slot(slot) else {
                    return json_error(404, "entity not found", alloc);
                };
                let Some(parent) = world.entity_by_slot(parent_slot) else {
                    return json_error(404, "parent entity not found", alloc);
                };
                let rel = world.child_of();
                return match world.add_pair(entity, rel, parent) {
                    Ok(()) => entity_json_response(world, 200, entity, alloc),
                    Err(err) => ecs_error_response(err, alloc),
                };
            }
            json_error(404, "not found", alloc)
        }
        _ => json_error(404, "not found", alloc),
    }
}

fn handle_post_entity<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    body: &[u8],
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let trimmed = trim_ascii_bytes(body);
    let parsed_doc = if trimmed.is_empty() {
        Value::Map(Map::new_in(alloc))
    } else {
        match blaze_json::read_in(trimmed, JsonLimits::SHIPPED, alloc) {
            Ok(v) => v,
            Err(_) => return json_error(400, "invalid JSON body", alloc),
        }
    };

    let Some(map) = parsed_doc.as_map() else {
        return json_error(400, "expected JSON object", alloc);
    };

    // Pre-validate name, parent, prefab, tags, and components before mutating the world.
    let mut name_opt: Option<&str> = None;
    if let Some(name_val) = map.get("name") {
        if !name_val.is_null() {
            let Some(s) = name_val.as_str() else {
                return json_error(400, "field `name` must be a string", alloc);
            };
            if !s.is_empty() {
                if world.entity_by_name(s).is_some() {
                    return json_error(400, "entity name is already in use", alloc);
                }
                name_opt = Some(s);
            }
        }
    }

    let mut parent_opt: Option<Entity> = None;
    if let Some(parent_val) = map.get("parent") {
        if !parent_val.is_null() {
            let Some(slot_u64) = parent_val.as_u64() else {
                return json_error(400, "field `parent` must be a non-negative integer slot", alloc);
            };
            let Ok(slot_u32) = u32::try_from(slot_u64) else {
                return json_error(400, "parent slot out of range", alloc);
            };
            let Some(parent_ent) = world.entity_by_slot(slot_u32) else {
                return json_error(400, "parent entity not found", alloc);
            };
            parent_opt = Some(parent_ent);
        }
    }

    let mut prefab_opt: Option<Entity> = None;
    if let Some(prefab_val) = map.get("prefab") {
        if !prefab_val.is_null() {
            let Some(slot_u64) = prefab_val.as_u64() else {
                return json_error(400, "field `prefab` must be a non-negative integer slot", alloc);
            };
            let Ok(slot_u32) = u32::try_from(slot_u64) else {
                return json_error(400, "prefab slot out of range", alloc);
            };
            let Some(prefab_ent) = world.entity_by_slot(slot_u32) else {
                return json_error(400, "prefab entity not found", alloc);
            };
            prefab_opt = Some(prefab_ent);
        }
    }

    let mut tag_names: OwnVec<&str, A> = OwnVec::new_in(alloc);
    if let Some(tags_val) = map.get("tags") {
        if !tags_val.is_null() {
            let Some(list) = tags_val.as_list() else {
                return json_error(400, "field `tags` must be a list of strings", alloc);
            };
            for item in list {
                let Some(tag_s) = item.as_str() else {
                    return json_error(400, "every entry in `tags` must be a string", alloc);
                };
                if !tag_s.is_empty() {
                    tag_names.try_push(tag_s).map_err(|r| r.why)?;
                }
            }
        }
    }

    let comps_map = match map.get("components") {
        Some(v) if !v.is_null() => match v.as_map() {
            Some(m) => Some(m),
            None => return json_error(400, "field `components` must be an object", alloc),
        },
        _ => None,
    };

    if let Some(cm) = comps_map {
        for (cname, cval) in cm.iter() {
            let Some(cid) = world.component_id_by_name(cname) else {
                return json_error(400, "unknown component name", alloc);
            };
            let Some(meta) = world.component_meta(cid) else {
                return json_error(400, "unknown component", alloc);
            };
            if let Err(err) = meta.validate_value(cval, alloc) {
                return reflect_error_response(err, alloc);
            }
        }
    }

    // Spawn or instantiate from prefab.
    let entity = match prefab_opt {
        Some(prefab_ent) => match world.instantiate(prefab_ent) {
            Ok(e) => e,
            Err(err) => return ecs_error_response(err, alloc),
        },
        None => match world.spawn() {
            Ok(e) => e,
            Err(err) => return ecs_error_response(err, alloc),
        },
    };

    if let Some(name) = name_opt {
        if let Err(err) = world.set_name(entity, name) {
            let _ = world.despawn(entity);
            return ecs_error_response(err, alloc);
        }
    }

    if let Some(parent_ent) = parent_opt {
        let rel = world.child_of();
        if let Err(err) = world.add_pair(entity, rel, parent_ent) {
            let _ = world.despawn(entity);
            return ecs_error_response(err, alloc);
        }
    }

    for &tag_s in tag_names.as_slice() {
        let tag_ent = match world.entity_by_name(tag_s) {
            Some(existing) => existing,
            None => match world.spawn_named(tag_s) {
                Ok(created) => created,
                Err(err) => {
                    let _ = world.despawn(entity);
                    return ecs_error_response(err, alloc);
                }
            },
        };
        if let Err(err) = world.add_tag(entity, tag_ent) {
            let _ = world.despawn(entity);
            return ecs_error_response(err, alloc);
        }
    }

    if let Some(cm) = comps_map {
        for (cname, cval) in cm.iter() {
            if let Err(err) = world.set_component_from_value(entity, cname, cval, alloc) {
                let _ = world.despawn(entity);
                return reflect_error_response(err, alloc);
            }
        }
    }

    entity_json_response(world, 201, entity, alloc)
}

fn handle_query<A: Allocator + Copy + 'static>(
    world: &World<A>,
    query_str: &str,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let mut with_raw: Option<&str> = None;
    let mut without_raw: Option<&str> = None;
    let mut include_prefabs = false;

    for pair in query_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        match k {
            "with" => with_raw = Some(v),
            "without" => without_raw = Some(v),
            "include_prefabs" => {
                include_prefabs = v == "true" || v == "1";
            }
            _ => {}
        }
    }

    let mut spec = QuerySpec::new_in(alloc);
    let mut impossible = false;

    if let Some(raw) = with_raw {
        for tok in raw.split(',') {
            let decoded = percent_decode(tok, alloc)?;
            let name = decoded.as_str().trim();
            if name.is_empty() {
                continue;
            }
            if name == "Prefab" {
                include_prefabs = true;
            }
            if let Some(cid) = world.component_id_by_name(name) {
                spec = match spec.with(Id::Component(cid)) {
                    Ok(s) => s,
                    Err(EcsError::Alloc(a)) => return Err(a),
                    Err(_) => return json_error(500, "query build error", alloc),
                };
            } else if let Some(tag_ent) = world.entity_by_name(name) {
                spec = match spec.with(Id::Tag(tag_ent)) {
                    Ok(s) => s,
                    Err(EcsError::Alloc(a)) => return Err(a),
                    Err(_) => return json_error(500, "query build error", alloc),
                };
            } else {
                impossible = true;
                break;
            }
        }
    }

    if !impossible {
        if let Some(raw) = without_raw {
            for tok in raw.split(',') {
                let decoded = percent_decode(tok, alloc)?;
                let name = decoded.as_str().trim();
                if name.is_empty() {
                    continue;
                }
                if let Some(cid) = world.component_id_by_name(name) {
                    spec = match spec.without(Id::Component(cid)) {
                        Ok(s) => s,
                        Err(EcsError::Alloc(a)) => return Err(a),
                        Err(_) => return json_error(500, "query build error", alloc),
                    };
                } else if let Some(tag_ent) = world.entity_by_name(name) {
                    spec = match spec.without(Id::Tag(tag_ent)) {
                        Ok(s) => s,
                        Err(EcsError::Alloc(a)) => return Err(a),
                        Err(_) => return json_error(500, "query build error", alloc),
                    };
                }
            }
        }
    }

    spec = spec.include_prefabs(include_prefabs);

    let mut matched_vals = OwnVec::new_in(alloc);
    if !impossible {
        let mut ents = match world.query_entities(&spec) {
            Ok(v) => v,
            Err(EcsError::Alloc(a)) => return Err(a),
            Err(_) => return json_error(500, "query execution failed", alloc),
        };
        ents.as_mut_slice().sort_unstable_by_key(|e| e.slot());
        for &e in ents.as_slice() {
            let val = match world.entity_to_value(e, alloc) {
                Ok(v) => v,
                Err(err) => return reflect_error_response(err, alloc),
            };
            matched_vals.try_push(val).map_err(|r| r.why)?;
        }
    }

    let count = matched_vals.len();
    let mut out_map = Map::new_in(alloc);
    let _ = out_map.insert(
        OwnString::try_from_str_in("count", alloc)?,
        Value::Int(Int::from_u64(u64::try_from(count).unwrap_or(u64::MAX))),
    );
    let _ = out_map.insert(
        OwnString::try_from_str_in("entities", alloc)?,
        Value::List(matched_vals),
    );
    json_value_response(200, &Value::Map(out_map), alloc)
}

// -----------------------------------------------------------------------------
// HTTP/1.1 Framing & Encoding Helpers
// -----------------------------------------------------------------------------

fn parse_http_request(raw: &[u8]) -> Option<(&str, &str, &[u8])> {
    let (header_end, body_start) = find_header_end(raw)?;
    let header_bytes = raw.get(..header_end)?;
    let header_str = core::str::from_utf8(header_bytes).ok()?;

    let mut lines = header_str.split('\n');
    let req_line = lines.next()?.trim_end_matches('\r');
    let mut req_parts = req_line.split_ascii_whitespace();
    let method = req_parts.next()?;
    let target = req_parts.next()?;
    let version = req_parts.next()?;
    if req_parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }

    let mut content_length: Option<usize> = None;
    for raw_line in lines {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((name, val)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(val.trim().parse::<usize>().ok()?);
            }
        }
    }

    let body = match content_length {
        Some(len) => {
            let end = body_start.checked_add(len)?;
            raw.get(body_start..end)?
        }
        None => raw.get(body_start..).unwrap_or(&[]),
    };

    Some((method, target, body))
}

fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    while i < raw.len() {
        if raw.get(i..i.saturating_add(4)) == Some(b"\r\n\r\n") {
            return Some((i, i.saturating_add(4)));
        }
        if raw.get(i..i.saturating_add(2)) == Some(b"\n\n") {
            return Some((i, i.saturating_add(2)));
        }
        i = i.saturating_add(1);
    }
    None
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    }
}

fn split_path_query(path: &str) -> (&str, &str) {
    match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    }
}

fn trim_ascii_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some(&first) = bytes.first() {
        if first.is_ascii_whitespace() {
            bytes = bytes.get(1..).unwrap_or(&[]);
        } else {
            break;
        }
    }
    while let Some(&last) = bytes.last() {
        if last.is_ascii_whitespace() {
            let new_len = bytes.len().saturating_sub(1);
            bytes = bytes.get(..new_len).unwrap_or(&[]);
        } else {
            break;
        }
    }
    bytes
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<OwnString<A>, AllocFailure> {
    let bytes = input.as_bytes();
    let mut raw = OwnVec::try_with_capacity_in(bytes.len(), alloc)?;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            if let (Some(&h), Some(&l)) =
                (bytes.get(i.saturating_add(1)), bytes.get(i.saturating_add(2)))
            {
                if let (Some(hi), Some(lo)) = (hex_nibble(h), hex_nibble(l)) {
                    raw.try_push((hi << 4) | lo).map_err(|r| r.why)?;
                    i = i.saturating_add(3);
                    continue;
                }
            }
        }
        raw.try_push(b).map_err(|r| r.why)?;
        i = i.saturating_add(1);
    }
    match core::str::from_utf8(raw.as_slice()) {
        Ok(valid) => OwnString::try_from_str_in(valid, alloc),
        Err(_) => OwnString::try_from_str_in(input, alloc),
    }
}

fn push_bytes<A: Allocator>(buf: &mut OwnVec<u8, A>, bytes: &[u8]) -> Result<(), AllocFailure> {
    buf.try_reserve(bytes.len())?;
    for &b in bytes {
        buf.try_push(b).map_err(|r| r.why)?;
    }
    Ok(())
}

fn entity_json_response<A: Allocator + Copy + 'static>(
    world: &World<A>,
    status: u16,
    entity: Entity,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    match world.entity_to_value(entity, alloc) {
        Ok(val) => json_value_response(status, &val, alloc),
        Err(err) => reflect_error_response(err, alloc),
    }
}

fn json_value_response<A: Allocator + Copy>(
    status: u16,
    val: &Value<A>,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    match blaze_json::to_string_in(val, JsonWriteOptions::COMPACT, alloc) {
        Ok(s) => {
            let mut body = OwnVec::new_in(alloc);
            push_bytes(&mut body, s.as_str().as_bytes())?;
            Ok(ExplorerResponse { status, content_type: CT_JSON, body })
        }
        Err(_) => json_error(500, "failed to serialize JSON response", alloc),
    }
}

fn json_error<A: Allocator + Copy>(
    status: u16,
    msg: &str,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let mut map = Map::new_in(alloc);
    let _ = map.insert(
        OwnString::try_from_str_in("error", alloc)?,
        Value::Text(OwnString::try_from_str_in(msg, alloc)?),
    );
    match blaze_json::to_string_in(&Value::Map(map), JsonWriteOptions::COMPACT, alloc) {
        Ok(s) => {
            let mut body = OwnVec::new_in(alloc);
            push_bytes(&mut body, s.as_str().as_bytes())?;
            Ok(ExplorerResponse { status, content_type: CT_JSON, body })
        }
        Err(_) => Ok(ExplorerResponse {
            status,
            content_type: CT_JSON,
            body: OwnVec::new_in(alloc),
        }),
    }
}

fn reflect_error_response<A: Allocator + Copy>(
    err: ReflectError,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let status = match err {
        ReflectError::Alloc(a) => return Err(a),
        ReflectError::EntityNotFound(_)
        | ReflectError::ComponentNotFound
        | ReflectError::MissingComponent(_) => 404,
        ReflectError::Shape(_)
        | ReflectError::NotReflected(_)
        | ReflectError::InvalidJson(_)
        | ReflectError::NotFinite
        | ReflectError::Ecs(_) => 400,
    };
    let mut msg = OwnString::new_in(alloc);
    let _ = write!(msg, "{err}");
    json_error(status, msg.as_str(), alloc)
}

fn ecs_error_response<A: Allocator + Copy>(
    err: EcsError,
    alloc: A,
) -> Result<ExplorerResponse<A>, AllocFailure> {
    let status = match err {
        EcsError::Alloc(a) => return Err(a),
        EcsError::DeadEntity(_) | EcsError::UnknownComponent(_) | EcsError::MissingComponent(_) => {
            404
        }
        _ => 400,
    };
    let mut msg = OwnString::new_in(alloc);
    let _ = write!(msg, "{err}");
    json_error(status, msg.as_str(), alloc)
}

// -----------------------------------------------------------------------------
// Self-Contained Dark-Mode Interactive Web UI (Zero External CDNs)
// -----------------------------------------------------------------------------

/// Self-contained, zero-CDN dark-mode Flecs-inspired interactive ECS Explorer Web UI served at
/// `GET /`.
pub const EXPLORER_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Blaze ECS Explorer</title>
<style>
:root {
  color-scheme: dark light;

  /* Blaze site warm monochrome grayscale ladder (styles/tokens.css) */
  --g-0: #0c0c0d;
  --g-1: #101011;
  --g-2: #141416;
  --g-3: #19191b;
  --g-4: #202022;
  --g-5: #28282b;
  --g-6: #333336;
  --g-7: #424245;
  --g-8: #58585b;
  --g-9: #737376;
  --g-10: #939290;
  --g-11: #b4b3af;
  --g-12: #d4d3cf;
  --g-13: #ebeae6;

  --bg-0: #0a0a0b;
  --bg: var(--g-0);
  --bg-1: var(--g-1);
  --bg-2: var(--g-2);
  --bg-3: var(--g-3);
  --bg-4: var(--g-4);

  --ink-1: var(--g-13);
  --ink-2: var(--g-11);
  --ink-3: var(--g-9);
  --ink-4: var(--g-8);

  --line: rgb(255 255 255 / 0.065);
  --line-2: rgb(255 255 255 / 0.115);
  --line-3: rgb(255 255 255 / 0.18);
  --s-tint: rgb(255 255 255 / 0.06);
  --s-tint-soft: rgb(255 255 255 / 0.032);

  /* Semantic-only color */
  --fire: #e0461a;
  --ok: #4cc27a;
  --danger: #ff7168;

  /* IntelliJ New UI dark syntax palette for Schemas */
  --code-bg: var(--g-2);
  --syn-text: #bcbec4;
  --syn-kw: #cf8e6d;
  --syn-ty: #c77dbb;
  --syn-mark: #7a7e85;

  --font-sans: -apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", system-ui, sans-serif;
  --font-mono: ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace;
}

@media (prefers-color-scheme: light) {
  :root:not([data-theme="dark"]) {
    --bg-0: #eae9e4;
    --bg: #f2f1ee;
    --bg-1: #f7f6f3;
    --bg-2: #ffffff;
    --bg-3: #ffffff;
    --bg-4: #ebeae6;
    --ink-1: #161617;
    --ink-2: #464649;
    --ink-3: #6c6c70;
    --ink-4: #9a9a9e;
    --line: rgb(0 0 0 / 0.075);
    --line-2: rgb(0 0 0 / 0.13);
    --line-3: rgb(0 0 0 / 0.22);
    --s-tint: rgb(0 0 0 / 0.055);
    --s-tint-soft: rgb(0 0 0 / 0.03);
    --fire: #d63c12;
    --ok: #1d8a4b;
    --danger: #cf2e26;
    --code-bg: #ffffff;
    --syn-text: #1c1c1e;
    --syn-kw: #a33e13;
    --syn-ty: #7b3579;
    --syn-mark: #7c7c80;
  }
}

:root[data-theme="light"] {
  --bg-0: #eae9e4;
  --bg: #f2f1ee;
  --bg-1: #f7f6f3;
  --bg-2: #ffffff;
  --bg-3: #ffffff;
  --bg-4: #ebeae6;
  --ink-1: #161617;
  --ink-2: #464649;
  --ink-3: #6c6c70;
  --ink-4: #9a9a9e;
  --line: rgb(0 0 0 / 0.075);
  --line-2: rgb(0 0 0 / 0.13);
  --line-3: rgb(0 0 0 / 0.22);
  --s-tint: rgb(0 0 0 / 0.055);
  --s-tint-soft: rgb(0 0 0 / 0.03);
  --fire: #d63c12;
  --ok: #1d8a4b;
  --danger: #cf2e26;
  --code-bg: #ffffff;
  --syn-text: #1c1c1e;
  --syn-kw: #a33e13;
  --syn-ty: #7b3579;
  --syn-mark: #7c7c80;
}

*, *::before, *::after { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body {
  background: var(--bg);
  color: var(--ink-2);
  font-family: var(--font-sans);
  font-size: 13px;
  line-height: 1.45;
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}
button, input, select { font: inherit; color: inherit; }
:focus-visible { outline: 1.5px solid var(--line-3); outline-offset: 1px; }
.mono { font-family: var(--font-mono); font-variant-numeric: tabular-nums; }
.ico { flex: none; display: inline-block; vertical-align: middle; }
.ico.fire { color: var(--fire); }
.muted { color: var(--ink-3); }
.faint { color: var(--ink-4); }
.strong { color: var(--ink-1); font-weight: 500; }

/* Frame: compact 232px sidebar + rounded pane */
.frame {
  display: grid;
  grid-template-columns: 232px minmax(0, 1fr);
  gap: 8px;
  height: 100vh;
  padding: 8px;
  overflow: hidden;
}

/* Sidebar */
.side {
  display: flex;
  flex-direction: column;
  min-height: 0;
  padding: 5px 2px 3px 4px;
}
.sidetop {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 4px 8px 5px;
}
.brand {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  color: var(--ink-1);
  text-decoration: none;
  font-weight: 600;
  font-size: 13.5px;
  letter-spacing: -0.02em;
}
.brand-logo { border-radius: 5px; flex: none; }
.brand .sub {
  color: var(--ink-4);
  font-weight: 400;
  font-size: 11.5px;
  font-family: var(--font-mono);
}

.iconbtn {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  padding: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-3);
  cursor: pointer;
  transition: background-color 0.1s, color 0.1s;
}
.iconbtn.sm { width: 22px; height: 22px; border-radius: 5px; }
.iconbtn:hover { background: var(--s-tint); color: var(--ink-1); }
.iconbtn.danger:hover { color: var(--danger); background: color-mix(in srgb, var(--danger) 11%, transparent); }

.searchbtn {
  display: flex;
  align-items: center;
  gap: 7px;
  width: calc(100% - 6px);
  height: 28px;
  margin: 0 3px 9px;
  padding: 0 7px 0 9px;
  border: 1px solid var(--line);
  border-radius: 7px;
  background: var(--bg-2);
  color: var(--ink-3);
  font-size: 12px;
  cursor: pointer;
  transition: border-color 0.12s, color 0.12s;
}
.searchbtn:hover { border-color: var(--line-2); color: var(--ink-1); }
.searchbtn span { flex: 1; text-align: left; }
.searchbtn kbd {
  padding: 1px 5px;
  border: 1px solid var(--line);
  border-radius: 4px;
  font-family: var(--font-mono);
  font-size: 10px;
  color: var(--ink-4);
}

.scrollbox {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  scrollbar-width: none;
  padding: 0 2px;
  mask-image: linear-gradient(to bottom, #000 calc(100% - 20px), transparent);
  -webkit-mask-image: linear-gradient(to bottom, #000 calc(100% - 20px), transparent);
}
.scrollbox::-webkit-scrollbar { display: none; }
.navlist { position: relative; padding-bottom: 18px; }
.glide-hov {
  position: absolute;
  top: 0;
  left: 0;
  z-index: 0;
  border-radius: 6px;
  pointer-events: none;
  background: var(--s-tint-soft);
  opacity: 0;
  transition: transform 0.13s cubic-bezier(0.34, 1.3, 0.5, 1), width 0.13s, height 0.08s, opacity 0.1s;
}

.sec + .sec { margin-top: 10px; }
.sec > summary { list-style: none; }
.sec > summary::-webkit-details-marker { display: none; }
.sechead {
  display: flex;
  align-items: center;
  gap: 5px;
  width: 100%;
  height: 24px;
  padding: 0 6px;
  border-radius: 6px;
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.02em;
  text-transform: uppercase;
  color: var(--ink-4);
  cursor: pointer;
  user-select: none;
  transition: color 0.12s;
}
.sechead:hover { color: var(--ink-2); }
.sechead span { flex: 1; }
.sechead .cnt { flex: none; font-family: var(--font-mono); font-size: 10.5px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.sechead .chev { transition: transform 0.16s; }
.sechead .sec-ico { opacity: 0.7; }
.sec:not([open]) > .sechead .chev { transform: rotate(-90deg); }

.row {
  position: relative;
  z-index: 1;
  display: flex;
  align-items: center;
  gap: 5px;
  height: 25px;
  padding: 0 6px 0 5px;
  border-radius: 6px;
  color: var(--ink-2);
  font-size: 12.5px;
  cursor: pointer;
  user-select: none;
  transition: color 0.1s, background-color 0.1s;
}
.row:hover { color: var(--ink-1); }
.row[aria-current="page"] { color: var(--ink-1); font-weight: 500; background: var(--s-tint); }
.row .label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.row .num { flex: none; font-family: var(--font-mono); font-size: 10.5px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.row[aria-current="page"] .num { color: var(--ink-3); }
.row .row-ico { color: var(--ink-4); }
.row[aria-current="page"] .row-ico:not(.fire) { color: var(--ink-2); }
.twisty {
  display: grid;
  place-items: center;
  width: 13px;
  height: 13px;
  flex: none;
  color: var(--ink-4);
  transition: transform 0.15s, color 0.1s;
}
.twisty:hover { color: var(--ink-1); }
.twisty.shut { transform: rotate(-90deg); }
.twisty.leaf { opacity: 0; pointer-events: none; }
.kids {
  margin-left: 11px;
  padding-left: 6px;
  border-left: 1px solid var(--line);
}
.row .inst {
  display: none;
  place-items: center;
  width: 18px;
  height: 18px;
  padding: 0;
  border: 1px solid var(--line-2);
  border-radius: 4px;
  background: var(--bg-3);
  color: var(--ink-2);
  cursor: pointer;
}
.row:hover .inst { display: inline-grid; }
.row .inst:hover { color: var(--ink-1); border-color: var(--line-3); }

.foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4px;
  padding: 7px 5px 2px;
  border-top: 1px solid var(--line);
  font-size: 11px;
  color: var(--ink-4);
}
.foot-stats { display: flex; align-items: center; gap: 10px; font-family: var(--font-mono); font-variant-numeric: tabular-nums; }
.fstat { display: inline-flex; align-items: center; gap: 4px; }
.foot-acts { display: flex; align-items: center; gap: 1px; }

/* Right rounded pane */
.pane {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  border-radius: 14px;
  background: var(--bg-1);
  box-shadow: inset 0 0 0 1px var(--line-2);
  overflow: hidden;
  position: relative;
}

/* Compact 40px topbar */
.topbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  height: 40px;
  padding: 0 12px 0 16px;
  border-bottom: 1px solid var(--line);
  flex: none;
}
.tabs { display: flex; align-items: center; gap: 2px; height: 100%; }
.tab {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 100%;
  padding: 0 9px;
  border: 0;
  background: transparent;
  color: var(--ink-3);
  font-size: 12.5px;
  font-weight: 500;
  cursor: pointer;
  transition: color 0.1s;
}
.tab:hover { color: var(--ink-1); }
.tab.active { color: var(--ink-1); }
.tab.active::after {
  content: '';
  position: absolute;
  left: 9px;
  right: 9px;
  bottom: -1px;
  height: 1.5px;
  background: var(--ink-1);
}
.tab .tcnt { font-family: var(--font-mono); font-size: 10.5px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.tab.active .tcnt { color: var(--ink-3); }

.top-acts { display: flex; align-items: center; gap: 3px; }
.ghost {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 25px;
  padding: 0 8px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-3);
  font-size: 12px;
  cursor: pointer;
  transition: background-color 0.1s, color 0.1s;
}
.ghost:hover { background: var(--s-tint); color: var(--ink-1); }
.ghost[hidden] { display: none; }
.ghost.on { color: var(--ok); background: color-mix(in srgb, var(--ok) 11%, transparent); }

/* Main workspace area */
.content {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 15px 22px 44px;
}
.content.fullbleed { padding: 0; overflow: hidden; display: flex; }
.col {
  width: 100%;
  max-width: 1120px;
  margin: 0 auto;
}
.content.fullbleed .col { max-width: none; height: 100%; display: flex; }

/* Dense entity header with icon-labeled stats */
.ehead {
  padding-bottom: 10px;
  border-bottom: 1px solid var(--line);
}
.crumb {
  display: flex;
  align-items: center;
  gap: 5px;
  margin-bottom: 3px;
  font-size: 11.5px;
  color: var(--ink-4);
}
.crumb a { color: inherit; text-decoration: none; cursor: pointer; }
.crumb a:hover { color: var(--ink-1); }
.crumb .sep { opacity: 0.45; }

.ehead-main {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}
.ehead-left {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px 10px;
  min-width: 0;
}
.ehead-acts { display: flex; align-items: center; gap: 2px; flex: none; }
.title-ico { color: var(--ink-3); }
.ehead h1 {
  margin: 0;
  font-size: 19px;
  line-height: 1.2;
  font-weight: 600;
  letter-spacing: -0.025em;
  color: var(--ink-1);
}
.stat-pills {
  display: inline-flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 5px;
}
.spill {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 21px;
  padding: 0 6px;
  border-radius: 5px;
  background: var(--s-tint-soft);
  color: var(--ink-3);
  font-family: var(--font-mono);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
}
.spill .ico { color: var(--ink-4); }

.isa-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 21px;
  padding: 0 7px;
  border-radius: 5px;
  background: color-mix(in srgb, var(--fire) 12%, transparent);
  color: var(--fire);
  font-size: 11.5px;
  font-weight: 500;
  text-decoration: none;
  cursor: pointer;
  transition: background-color 0.1s;
}
.isa-chip:hover { background: color-mix(in srgb, var(--fire) 20%, transparent); }

/* Clean icon-labeled properties strip (ZERO ugly selects, ZERO hashtags!) */
.props-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px 10px;
  margin-top: 8px;
}
.prop-group {
  display: inline-flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 5px;
}
.prop-ico { color: var(--ink-4); }
.prop-sep { width: 1px; height: 14px; background: var(--line-2); margin: 0 2px; }

.flat-select {
  height: 23px;
  padding: 0 7px;
  border: 1px solid transparent;
  border-radius: 5px;
  background-color: transparent;
  color: var(--ink-2);
  font-size: 12px;
  cursor: pointer;
  transition: background-color 0.1s, border-color 0.1s, color 0.1s;
}
.flat-select:hover { background-color: var(--s-tint); color: var(--ink-1); }
.flat-select:focus { background-color: var(--bg-0); border-color: var(--line-2); color: var(--ink-1); outline: none; }
.flat-select.framed { background-color: var(--bg-2); border-color: var(--line); }
.flat-select.framed:hover { border-color: var(--line-2); }

/* Sleek borderless flat-radius tags — NO hashtags, hover-reveal × */
.tag {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 21px;
  padding: 0 7px;
  border-radius: 5px;
  background: var(--s-tint);
  color: var(--ink-2);
  font-size: 11.5px;
  font-weight: 500;
  transition: background-color 0.1s, color 0.1s;
}
.tag:hover { background: var(--bg-4); color: var(--ink-1); }
.tag button {
  display: inline-grid;
  place-items: center;
  width: 13px;
  height: 13px;
  margin-right: -3px;
  padding: 0;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: var(--ink-4);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.1s, color 0.1s, background-color 0.1s;
}
.tag:hover button, .tag button:focus { opacity: 1; }
.tag button:hover { color: var(--danger); background: color-mix(in srgb, var(--danger) 14%, transparent); }

.tag-add {
  height: 21px;
  width: 54px;
  padding: 0 6px;
  border: 1px solid transparent;
  border-radius: 5px;
  background: transparent;
  color: var(--ink-2);
  font-size: 11.5px;
  transition: width 0.16s, background-color 0.1s, border-color 0.1s;
}
.tag-add::placeholder { color: var(--ink-4); }
.tag-add:hover { background: var(--s-tint-soft); }
.tag-add:focus {
  width: 114px;
  background: var(--bg-0);
  border-color: var(--line-2);
  color: var(--ink-1);
  outline: none;
}

/* Components toolbar */
.comp-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin: 11px 0 8px;
}
.comp-bar-l, .comp-bar-r { display: flex; align-items: center; gap: 6px; }
.comp-bar-title {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.02em;
  text-transform: uppercase;
  color: var(--ink-4);
}
.cnt-pill { font-size: 11px; color: var(--ink-3); }

/* Dense 2-column responsive component grid */
.comp-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(350px, 1fr));
  gap: 9px;
  align-items: start;
}
.comp-card {
  border: 1px solid var(--line);
  border-radius: 9px;
  background: var(--bg-2);
  overflow: hidden;
  transition: border-color 0.12s;
}
.comp-card:hover { border-color: var(--line-2); }
.comp-card > summary {
  list-style: none;
  display: flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  padding: 0 9px;
  background: color-mix(in srgb, var(--ink-1) 2%, transparent);
  cursor: pointer;
  user-select: none;
}
.comp-card > summary::-webkit-details-marker { display: none; }
.comp-card[open] > summary { border-bottom: 1px solid var(--line); }
.comp-card .chev { color: var(--ink-4); transition: transform 0.14s; }
.comp-card:not([open]) .chev { transform: rotate(-90deg); }
.comp-ico { color: var(--ink-3); }
.comp-name { font-size: 12.5px; font-weight: 600; color: var(--ink-1); letter-spacing: -0.01em; }
.comp-size { font-family: var(--font-mono); font-size: 10.5px; color: var(--ink-4); }
.comp-status { margin-left: auto; font-family: var(--font-mono); font-size: 10.5px; color: var(--ink-4); }
.comp-status.ok { color: var(--ok); }
.comp-status.err { color: var(--danger); }
.comp-copy {
  display: grid;
  place-items: center;
  width: 20px;
  height: 20px;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--ink-4);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.1s, color 0.1s, background-color 0.1s;
}
.comp-card:hover .comp-copy, .comp-copy:focus { opacity: 1; }
.comp-copy:hover { color: var(--ink-1); background: var(--s-tint); }

.comp-body {
  padding: 8px 9px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

/* Structural horizontal numeric strips (2..4 cols) + 2-col scalar property grids */
.vec-row { display: grid; gap: 6px; }
.vec-row.n2 { grid-template-columns: repeat(2, minmax(0, 1fr)); }
.vec-row.n3 { grid-template-columns: repeat(3, minmax(0, 1fr)); }
.vec-row.n4 { grid-template-columns: repeat(4, minmax(0, 1fr)); }
.kv-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 6px; }
.kv-grid.n1 { grid-template-columns: 1fr; }
.well.well-wide { grid-column: 1 / -1; }

/* Tactile recessed input well */
.well {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 27px;
  padding: 0 7px 0 8px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--bg-0);
  min-width: 0;
  transition: border-color 0.12s;
}
.well:hover { border-color: var(--line-2); }
.well:focus-within { border-color: var(--line-3); }
.well.scrubbing { border-color: var(--fire); }
.well.saved { border-color: color-mix(in srgb, var(--ok) 65%, var(--line)); }
.well.err { border-color: var(--danger); }

.well-k {
  flex: none;
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--ink-3);
  user-select: none;
  white-space: nowrap;
  max-width: 68%;
  overflow: hidden;
  text-overflow: ellipsis;
}
.well-k.scrub { cursor: ew-resize; }
.well-k.scrub:hover { color: var(--ink-1); }
.well.scrubbing .well-k { color: var(--fire); }

.well input[type="number"],
.well input[type="text"] {
  flex: 1;
  min-width: 0;
  width: 100%;
  height: 100%;
  padding: 0;
  border: 0;
  background: transparent;
  color: var(--ink-1);
  font-family: var(--font-mono);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  text-align: right;
  outline: none;
}
.well input[type="number"]::-webkit-inner-spin-button,
.well input[type="number"]::-webkit-outer-spin-button { -webkit-appearance: none; margin: 0; }
.well input[type="number"] { -moz-appearance: textfield; }
.well input[type="text"] { text-align: left; }

.well-bool { justify-content: space-between; cursor: pointer; }
.well-bool .well-k { cursor: pointer; }
.sw-wrap { display: inline-flex; align-items: center; gap: 6px; flex: none; }
.sw-wrap input { position: absolute; opacity: 0; pointer-events: none; }
.sw-track {
  position: relative;
  width: 26px;
  height: 15px;
  border-radius: 999px;
  background: var(--g-6);
  transition: background-color 0.14s;
}
:root[data-theme="light"] .sw-track { background: #cfcec9; }
.sw-track::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: 11px;
  height: 11px;
  border-radius: 50%;
  background: #fff;
  transition: transform 0.14s;
}
.sw-wrap input:checked + .sw-track { background: var(--fire); }
.sw-wrap input:checked + .sw-track::after { transform: translateX(11px); }
.sw-val { font-family: var(--font-mono); font-size: 11px; color: var(--ink-3); min-width: 28px; text-align: right; }

/* WebGPU full-bleed stage */
.gpu-stage {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 460px;
  overflow: hidden;
  user-select: none;
}
.gpu-canvas, .gpu-labels {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  display: block;
}
.gpu-labels { pointer-events: none; }
.gpu-canvas { cursor: grab; }
.gpu-canvas.dragging { cursor: grabbing; }
.gpu-canvas.pointing { cursor: pointer; }

.gpu-hud-tl {
  position: absolute;
  top: 12px;
  left: 12px;
  display: flex;
  align-items: center;
  gap: 6px;
  z-index: 5;
}
.seg {
  display: inline-flex;
  padding: 2px;
  border: 1px solid var(--line-2);
  border-radius: 8px;
  background: color-mix(in srgb, var(--bg-2) 86%, transparent);
  backdrop-filter: blur(10px);
}
.seg-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 24px;
  padding: 0 9px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-3);
  font-size: 11.5px;
  font-weight: 500;
  cursor: pointer;
}
.seg-btn:hover { color: var(--ink-1); }
.seg-btn.active { background: var(--s-tint); color: var(--ink-1); }

.gpu-pill {
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  border: 1px solid var(--line-2);
  border-radius: 8px;
  background: color-mix(in srgb, var(--bg-2) 86%, transparent);
  backdrop-filter: blur(10px);
  color: var(--ink-2);
  cursor: pointer;
}
.gpu-pill:hover { color: var(--ink-1); border-color: var(--line-3); }
.gpu-badge {
  padding: 3px 8px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: color-mix(in srgb, var(--bg-2) 78%, transparent);
  backdrop-filter: blur(8px);
  font-family: var(--font-mono);
  font-size: 10.5px;
  color: var(--ink-3);
}

.gpu-hud-bl {
  position: absolute;
  bottom: 12px;
  left: 12px;
  z-index: 5;
  padding: 8px 10px;
  border: 1px solid var(--line-2);
  border-radius: 9px;
  background: color-mix(in srgb, var(--bg-2) 88%, transparent);
  backdrop-filter: blur(10px);
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.gpu-sel-row { display: flex; align-items: center; gap: 7px; }
.gpu-sel-name { font-size: 12.5px; font-weight: 600; color: var(--ink-1); }
.gpu-sel-coords { font-size: 11px; color: var(--ink-3); }
.gpu-hud-br {
  position: absolute;
  bottom: 12px;
  right: 14px;
  z-index: 5;
  font-family: var(--font-mono);
  font-size: 10.5px;
  color: var(--ink-4);
}
.gpu-err {
  position: absolute;
  inset: 0;
  display: grid;
  place-items: center;
  padding: 24px;
  text-align: center;
  color: var(--ink-3);
  font-size: 13px;
  background: var(--bg-1);
}
.gpu-err[hidden] { display: none; }

/* Buttons */
.btn-subtle {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 23px;
  padding: 0 8px;
  border: 1px solid var(--line-2);
  border-radius: 5px;
  background: var(--bg-2);
  color: var(--ink-2);
  font-size: 11.5px;
  font-weight: 500;
  cursor: pointer;
}
.btn-subtle:hover { background: var(--s-tint); border-color: var(--line-3); color: var(--ink-1); }
.btn-primary {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 28px;
  padding: 0 12px;
  border: 0;
  border-radius: 6px;
  background: var(--ink-1);
  color: var(--bg);
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
}
.btn-primary:hover { opacity: 0.88; }

/* Archetypes / Query / Schemas */
.view-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding-bottom: 10px;
  border-bottom: 1px solid var(--line);
  margin-bottom: 12px;
}
.vh-l { display: flex; align-items: center; gap: 7px; }
.view-head h1 { margin: 0; font-size: 17px; font-weight: 600; letter-spacing: -0.02em; color: var(--ink-1); }
.emeta { font-family: var(--font-mono); font-size: 11px; color: var(--ink-4); }
.etable { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.etable th {
  text-align: left;
  padding: 6px 8px;
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.02em;
  text-transform: uppercase;
  color: var(--ink-4);
  border-bottom: 1px solid var(--line);
}
.etable td { padding: 7px 8px; border-bottom: 1px solid var(--line); vertical-align: middle; }
.etable tr:hover td { background: var(--s-tint-soft); }
.sig-list { display: flex; flex-wrap: wrap; gap: 4px; }
.sig-chip {
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--s-tint);
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--ink-2);
}
.sig-chip.rel { background: color-mix(in srgb, var(--fire) 11%, transparent); color: var(--fire); }

.qbar { display: flex; gap: 8px; margin-bottom: 12px; }
.qinput {
  flex: 1;
  height: 28px;
  padding: 0 9px;
  border: 1px solid var(--line-2);
  border-radius: 6px;
  background: var(--bg-0);
  color: var(--ink-1);
  font-family: var(--font-mono);
  font-size: 12px;
  outline: none;
}
.qinput:focus { border-color: var(--line-3); }
.qinput::placeholder { color: var(--ink-4); }

.schema-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 9px; }
.schema-block {
  margin: 0;
  padding: 11px 13px;
  border: 1px solid var(--line);
  border-radius: 9px;
  background: var(--code-bg);
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 1.55;
  color: var(--syn-text);
  overflow-x: auto;
}
.schema-block .kw { color: var(--syn-kw); }
.schema-block .ty { color: var(--syn-ty); }
.schema-block .cm { color: var(--syn-mark); }

/* Floating Cmd+F .page-find bar (styles/intro.css:269) */
.page-find {
  position: fixed;
  top: 56px;
  right: 20px;
  z-index: 60;
  display: flex;
  align-items: center;
  gap: 2px;
  height: 36px;
  padding: 0 4px 0 9px;
  background: var(--bg-3);
  border: 1px solid var(--line-2);
  border-radius: 9px;
  box-shadow: 0 8px 24px rgb(0 0 0 / 0.42);
  font: 12.5px/1 var(--font-sans);
  color: var(--ink-1);
}
.page-find[hidden] { display: none; }
.page-find:focus-within { border-color: var(--line-3); }
.page-find input, .page-find input:focus {
  width: 184px;
  height: 100%;
  padding: 0 6px;
  border: 0;
  outline: none;
  background: transparent;
  color: var(--ink-1);
  font: inherit;
}
.page-find input::placeholder { color: var(--ink-4); }
.page-find-glass { display: flex; color: var(--ink-4); }
.page-find-count {
  min-width: 48px;
  text-align: right;
  padding-right: 5px;
  color: var(--ink-3);
  font-size: 11.5px;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.page-find[data-none] .page-find-count { color: var(--fire); }
.page-find-sep { width: 1px; height: 16px; margin: 0 3px; background: var(--line-2); }
.page-find button {
  display: grid;
  place-items: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 5px;
  background: transparent;
  color: var(--ink-2);
  cursor: pointer;
}
.page-find button:hover:not(:disabled) { background: var(--s-tint); color: var(--ink-1); }
.page-find button:disabled { color: var(--ink-4); opacity: 0.5; cursor: default; }

mark.fm { background: color-mix(in srgb, var(--fire) 22%, transparent); color: inherit; border-radius: 3px; padding: 0 1px; }
mark.fm.cur { background: color-mix(in srgb, var(--fire) 46%, transparent); color: var(--ink-1); }
.row.find-hit { background: color-mix(in srgb, var(--fire) 12%, transparent); }
.row.find-cur { background: color-mix(in srgb, var(--fire) 24%, transparent); color: var(--ink-1); }

/* Modals */
.modal-bg {
  position: fixed;
  inset: 0;
  z-index: 70;
  display: none;
  align-items: flex-start;
  justify-content: center;
  padding-top: 11vh;
  background: rgb(0 0 0 / 0.48);
}
.modal-bg.open { display: flex; }
.modal {
  width: min(500px, calc(100vw - 24px));
  max-height: 76vh;
  display: flex;
  flex-direction: column;
  border-radius: 12px;
  background: var(--bg-3);
  color: var(--ink-2);
  box-shadow: 0 0 0 1px var(--line-2), 0 28px 64px -18px rgb(0 0 0 / 0.55);
  overflow: hidden;
}
.modal-hd {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 11px 14px;
  border-bottom: 1px solid var(--line);
  font-weight: 600;
  font-size: 13px;
  color: var(--ink-1);
}
.modal-bd { padding: 12px 14px; overflow-y: auto; display: flex; flex-direction: column; gap: 9px; }
.modal-bd label { display: flex; flex-direction: column; gap: 4px; font-size: 11.5px; color: var(--ink-3); }
.modal-bd pre {
  margin: 0;
  padding: 11px;
  border-radius: 7px;
  background: var(--code-bg);
  border: 1px solid var(--line);
  font-family: var(--font-mono);
  font-size: 11.5px;
  color: var(--ink-1);
  overflow: auto;
  max-height: 52vh;
  white-space: pre-wrap;
}
.modal-ft { display: flex; justify-content: flex-end; gap: 6px; padding: 10px 14px; border-top: 1px solid var(--line); }
.empty { padding: 28px 0; color: var(--ink-4); font-size: 12.5px; }
.empty-sm { padding: 8px 8px; color: var(--ink-4); font-size: 12px; }

@media (max-width: 820px) {
  .frame { grid-template-columns: 1fr; padding: 6px; }
  .side { height: auto; }
  .comp-grid, .schema-grid { grid-template-columns: 1fr; }
}
</style>
</head>
<body>

<div class="frame">
  <aside class="side" id="sideRoot"></aside>

  <div class="pane">
    <div class="topbar">
      <div class="tabs" id="tabs">
        <button class="tab active" data-view="inspector" type="button">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h10M18 7h2M4 12h4M12 12h8M4 17h12"/><circle cx="16" cy="7" r="2"/><circle cx="10" cy="12" r="2"/><circle cx="18" cy="17" r="2"/></svg>
          <span>Inspector</span>
        </button>
        <button class="tab" data-view="graph" type="button">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="7" r="2.4"/><circle cx="18" cy="6" r="2.4"/><circle cx="16" cy="18" r="2.4"/><circle cx="6.5" cy="17.5" r="2"/><path d="M8.3 6.8l7.3-.6M7.6 8.8l6.7 7.4M17.4 8.3l-1 7.3M8.5 17.6l5.1.3"/></svg>
          <span>Graph</span>
        </button>
        <button class="tab" data-view="archetypes" type="button">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M3.5 9.5h17M3.5 14.5h17M9.5 9.5v10"/></svg>
          <span>Archetypes</span>
          <span class="tcnt" id="tabArchCnt">0</span>
        </button>
        <button class="tab" data-view="query" type="button">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5.5h16l-6.2 7.2v5.3l-3.6 1.5v-6.8L4 5.5z"/></svg>
          <span>Query</span>
        </button>
        <button class="tab" data-view="schemas" type="button">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><path d="M8.5 7.5L4 12l4.5 4.5M15.5 7.5L20 12l-4.5 4.5M13.5 5.5l-3 13"/></svg>
          <span>Schemas</span>
          <span class="tcnt" id="tabCompCnt">0</span>
        </button>
      </div>
      <div class="top-acts">
        <button class="ghost" id="btnBuiltins" type="button" hidden title="Toggle built-in entities">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 12s3.5-6.5 9.5-6.5S21.5 12 21.5 12s-3.5 6.5-9.5 6.5S2.5 12 2.5 12z"/><circle cx="12" cy="12" r="2.5"/></svg>
        </button>
        <button class="ghost" id="btnAuto" type="button" title="Toggle auto-refresh">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="2.5" fill="currentColor" stroke="none"/><path d="M7.8 7.8a6 6 0 0 0 0 8.4M16.2 7.8a6 6 0 0 1 0 8.4"/></svg>
          <span>Auto</span>
        </button>
        <button class="iconbtn" id="btnRefresh" type="button" title="Refresh world">
          <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3L19.5 9"/><path d="M19.5 4.5V9H15"/></svg>
        </button>
      </div>
    </div>

    <div class="content" id="contentArea">
      <div class="col" id="viewRoot"></div>
    </div>
  </div>
</div>

<!-- Floating Cmd+F find bar (styles/intro.css:269) -->
<div class="page-find" id="pageFind" hidden role="search" aria-label="Find in world">
  <span class="page-find-glass" aria-hidden="true">
    <svg class="ico" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"><circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5L20.5 20.5"/></svg>
  </span>
  <input id="findInput" type="text" placeholder="Find entity, component, field…" autocomplete="off" spellcheck="false">
  <span class="page-find-count" id="findCount"></span>
  <span class="page-find-sep"></span>
  <button id="findPrev" type="button" title="Previous match (Shift+Enter)" disabled>
    <svg class="ico" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6 15l6-6 6 6"/></svg>
  </button>
  <button id="findNext" type="button" title="Next match (Enter)" disabled>
    <svg class="ico" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>
  </button>
  <button id="findClose" type="button" title="Close (Escape)">
    <svg class="ico" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg>
  </button>
</div>

<!-- Export modal -->
<div class="modal-bg" id="exportModal">
  <div class="modal">
    <div class="modal-hd"><span id="exportTitle">Export</span><button class="iconbtn" id="btnCloseExport" type="button">×</button></div>
    <div class="modal-bd"><pre id="exportPre"></pre></div>
    <div class="modal-ft">
      <button class="btn-subtle" id="btnCopyExport" type="button">Copy</button>
      <button class="btn-primary" id="btnDoneExport" type="button">Done</button>
    </div>
  </div>
</div>

<!-- Spawn modal -->
<div class="modal-bg" id="spawnModal">
  <div class="modal">
    <div class="modal-hd"><span>Spawn Entity</span><button class="iconbtn" id="btnCloseSpawn" type="button">×</button></div>
    <div class="modal-bd">
      <label>Name<input class="qinput" id="spName" placeholder="e.g. Scout_03"></label>
      <label>Parent<select class="qinput" id="spParent"></select></label>
      <label>Prefab (IsA)<select class="qinput" id="spPrefab"></select></label>
      <label>Tags (comma-separated)<input class="qinput" id="spTags" placeholder="Friendly, Active"></label>
      <span id="spErr" style="color:var(--danger);font-size:11.5px"></span>
    </div>
    <div class="modal-ft">
      <button class="btn-subtle" id="btnCancelSpawn" type="button">Cancel</button>
      <button class="btn-primary" id="btnConfirmSpawn" type="button">Spawn</button>
    </div>
  </div>
</div>

<script type="module">
// Minimal JS + WebGPU host shim over `blaze-ecs-wasm` (zero UI rendering logic in JS!)
const $ = (id) => document.getElementById(id);
const enc = new TextEncoder();
const dec = new TextDecoder();

let W = null, H = 0, worldJson = null, statusObj = {}, autoTimer = null;

// --- WASM C-ABI helpers ---
function wasmCallBytes(fn, str) {
  const bytes = enc.encode(str);
  const ptr = W.explorer_alloc(H, bytes.length || 1);
  if (bytes.length) new Uint8Array(W.memory.buffer, ptr, bytes.length).set(bytes);
  const res = fn(H, ptr, bytes.length);
  W.explorer_free(H, ptr, bytes.length || 1);
  return res;
}
function wasmReadOut(len) {
  if (!len) return "";
  return dec.decode(new Uint8Array(W.memory.buffer, W.explorer_out_ptr(H), len));
}
function dispatch(obj) {
  wasmCallBytes(W.explorer_action, JSON.stringify(obj));
  renderAll();
}

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = typeof body === "string" ? body : JSON.stringify(body);
  }
  const r = await fetch(path, opts);
  const txt = await r.text();
  let json = null;
  try { json = JSON.parse(txt); } catch (_) {}
  return { ok: r.ok, status: r.status, text: txt, json };
}

// --- WebGPU Instanced SDF Renderer (WGSL emitted from Rust via `gpu::emit::emit_wgsl` inside WASM!) ---
const gpu = {
  dev: null, ctx: null, fmt: null,
  edgePipe: null, nodePipe: null,
  camBuf: null, edgeBg: null, nodeBg: null,
  nodeBuf: null, edgeBuf: null, nodeCap: 0, edgeCap: 0,
  raf: 0, lastT: 0, mountedCanvas: null,
  cam: { cx: 0, cy: 0, zoom: 1.0 },
  drag: { mode: null, slot: -1, sx: 0, sy: 0, cx: 0, cy: 0, moved: false },
  didFit: false
};

function rebuildGpuBindGroups() {
  if (!gpu.dev) return;
  if (gpu.edgeBuf && gpu.edgePipe) {
    gpu.edgeBg = gpu.dev.createBindGroup({
      layout: gpu.edgePipe.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: gpu.camBuf } },
        { binding: 1, resource: { buffer: gpu.edgeBuf } }
      ]
    });
  }
  if (gpu.nodeBuf && gpu.nodePipe) {
    gpu.nodeBg = gpu.dev.createBindGroup({
      layout: gpu.nodePipe.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: gpu.camBuf } },
        { binding: 1, resource: { buffer: gpu.nodeBuf } }
      ]
    });
  }
}

async function initGpuDevice() {
  if (gpu.dev) return true;
  if (!navigator.gpu) return false;
  const adapter = await navigator.gpu.requestAdapter();
  if (!adapter) return false;
  gpu.dev = await adapter.requestDevice();
  gpu.fmt = navigator.gpu.getPreferredCanvasFormat();
  gpu.camBuf = gpu.dev.createBuffer({ size: 32, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });

  const blend = {
    color: { srcFactor: "src-alpha", dstFactor: "one-minus-src-alpha", operation: "add" },
    alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha", operation: "add" }
  };
  const edgeCode = wasmReadOut(W.explorer_wgsl_edge(H));
  const nodeCode = wasmReadOut(W.explorer_wgsl_node(H));
  const edgeMod = gpu.dev.createShaderModule({ code: edgeCode });
  const nodeMod = gpu.dev.createShaderModule({ code: nodeCode });
  gpu.edgePipe = gpu.dev.createRenderPipeline({
    layout: "auto",
    vertex: { module: edgeMod, entryPoint: "edge_vs" },
    fragment: { module: edgeMod, entryPoint: "edge_fs", targets: [{ format: gpu.fmt, blend }] },
    primitive: { topology: "triangle-list" }
  });
  gpu.nodePipe = gpu.dev.createRenderPipeline({
    layout: "auto",
    vertex: { module: nodeMod, entryPoint: "node_vs" },
    fragment: { module: nodeMod, entryPoint: "node_fs", targets: [{ format: gpu.fmt, blend }] },
    primitive: { topology: "triangle-list" }
  });
  return true;
}

function fitCamera() {
  const stage = $("gpuStage");
  if (!stage || !W) return;
  const r = stage.getBoundingClientRect();
  const [cx, cy, sw, sh] = JSON.parse(wasmReadOut(W.explorer_fit_graph(H)) || "[0,0,320,240]");
  gpu.cam.cx = cx;
  gpu.cam.cy = cy;
  gpu.cam.zoom = Math.max(0.25, Math.min(3.2, Math.min((r.width || 640) / sw, (r.height || 420) / sh) * 0.86));
  gpu.didFit = true;
}

function screenToWorld(canvas, clientX, clientY) {
  const r = canvas.getBoundingClientRect();
  return [
    gpu.cam.cx + (clientX - r.left - r.width * 0.5) / gpu.cam.zoom,
    gpu.cam.cy + (clientY - r.top - r.height * 0.5) / gpu.cam.zoom
  ];
}

function stopGpuLoop() {
  if (gpu.raf) { cancelAnimationFrame(gpu.raf); gpu.raf = 0; }
  gpu.mountedCanvas = null;
}

async function mountWebGpu() {
  const canvas = $("gpuCanvas");
  const lCanvas = $("gpuLabels");
  if (!canvas || !lCanvas) { stopGpuLoop(); return; }
  if (gpu.mountedCanvas === canvas) return;
  stopGpuLoop();
  gpu.mountedCanvas = canvas;

  const ok = await initGpuDevice();
  if (!ok) {
    const err = $("gpuErr");
    if (err) { err.hidden = false; err.textContent = "WebGPU unavailable in this browser (requires navigator.gpu)."; }
    return;
  }
  gpu.ctx = canvas.getContext("webgpu");
  gpu.ctx.configure({ device: gpu.dev, format: gpu.fmt, alphaMode: "premultiplied" });

  // Warm up layout a few frames on first open then fit cleanly
  for (let i = 0; i < 40; i++) W.explorer_graph_step(H, 0.016);
  if (!gpu.didFit) fitCamera();

  canvas.addEventListener("pointermove", (ev) => {
    const [wx, wy] = screenToWorld(canvas, ev.clientX, ev.clientY);
    if (gpu.drag.mode === "node") {
      if (Math.hypot(ev.clientX - gpu.drag.sx, ev.clientY - gpu.drag.sy) > 3) gpu.drag.moved = true;
      W.explorer_graph_drag(H, gpu.drag.slot, wx, wy, 1);
    } else if (gpu.drag.mode === "pan") {
      gpu.cam.cx = gpu.drag.cx - (ev.clientX - gpu.drag.sx) / gpu.cam.zoom;
      gpu.cam.cy = gpu.drag.cy - (ev.clientY - gpu.drag.sy) / gpu.cam.zoom;
    } else {
      const hit = W.explorer_graph_pick(H, wx, wy, 7 / gpu.cam.zoom);
      W.explorer_graph_hover(H, hit);
      canvas.classList.toggle("pointing", hit >= 0);
    }
  });
  canvas.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    canvas.setPointerCapture(ev.pointerId);
    const [wx, wy] = screenToWorld(canvas, ev.clientX, ev.clientY);
    const hit = W.explorer_graph_pick(H, wx, wy, 7 / gpu.cam.zoom);
    gpu.drag = { mode: hit >= 0 ? "node" : "pan", slot: hit, sx: ev.clientX, sy: ev.clientY, cx: gpu.cam.cx, cy: gpu.cam.cy, moved: false };
    if (hit >= 0) {
      W.explorer_graph_drag(H, hit, wx, wy, 1);
      dispatch({ op: "select", slot: hit, openInspector: false });
    }
    canvas.classList.add("dragging");
  });
  const endDrag = () => {
    if (gpu.drag.mode === "node") W.explorer_graph_drag(H, gpu.drag.slot, 0, 0, 0);
    gpu.drag.mode = null;
    canvas.classList.remove("dragging");
  };
  canvas.addEventListener("pointerup", endDrag);
  canvas.addEventListener("pointercancel", endDrag);
  canvas.addEventListener("dblclick", (ev) => {
    const [wx, wy] = screenToWorld(canvas, ev.clientX, ev.clientY);
    const hit = W.explorer_graph_pick(H, wx, wy, 8 / gpu.cam.zoom);
    if (hit >= 0) dispatch({ op: "select", slot: hit, openInspector: true });
  });
  canvas.addEventListener("wheel", (ev) => {
    ev.preventDefault();
    const [wx, wy] = screenToWorld(canvas, ev.clientX, ev.clientY);
    const factor = Math.exp(-ev.deltaY * 0.0014);
    gpu.cam.zoom = Math.max(0.2, Math.min(5.0, gpu.cam.zoom * factor));
    const [wx2, wy2] = screenToWorld(canvas, ev.clientX, ev.clientY);
    gpu.cam.cx += wx - wx2;
    gpu.cam.cy += wy - wy2;
  }, { passive: false });

  gpu.lastT = performance.now();
  const frame = (now) => {
    if (gpu.mountedCanvas !== canvas) return;
    const dt = Math.min(0.05, Math.max(0.001, (now - gpu.lastT) / 1000));
    gpu.lastT = now;

    const r = canvas.getBoundingClientRect();
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const pw = Math.max(1, Math.round(r.width * dpr));
    const ph = Math.max(1, Math.round(r.height * dpr));
    if (canvas.width !== pw || canvas.height !== ph) {
      canvas.width = pw; canvas.height = ph;
      lCanvas.width = pw; lCanvas.height = ph;
    }

    // Step relational ECS physics & pack GPU buffers in WASM
    W.explorer_graph_step(H, dt);
    const nLen = W.explorer_graph_nodes_len(H);
    const eLen = W.explorer_graph_edges_len(H);
    const nView = new Float32Array(W.memory.buffer, W.explorer_graph_nodes_ptr(H), nLen);
    const eView = new Float32Array(W.memory.buffer, W.explorer_graph_edges_ptr(H), eLen);

    let resized = false;
    if (Math.max(32, nView.byteLength) > gpu.nodeCap) {
      if (gpu.nodeBuf) gpu.nodeBuf.destroy();
      gpu.nodeCap = Math.max(4096, nView.byteLength * 2);
      gpu.nodeBuf = gpu.dev.createBuffer({ size: gpu.nodeCap, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
      resized = true;
    }
    if (Math.max(32, eView.byteLength) > gpu.edgeCap) {
      if (gpu.edgeBuf) gpu.edgeBuf.destroy();
      gpu.edgeCap = Math.max(4096, eView.byteLength * 2);
      gpu.edgeBuf = gpu.dev.createBuffer({ size: gpu.edgeCap, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
      resized = true;
    }
    if (resized) rebuildGpuBindGroups();
    if (nLen) gpu.dev.queue.writeBuffer(gpu.nodeBuf, 0, nView);
    if (eLen) gpu.dev.queue.writeBuffer(gpu.edgeBuf, 0, eView);

    const sx = (2 * gpu.cam.zoom) / Math.max(r.width, 1);
    const sy = (2 * gpu.cam.zoom) / Math.max(r.height, 1);
    gpu.dev.queue.writeBuffer(gpu.camBuf, 0, new Float32Array([gpu.cam.cx, gpu.cam.cy, sx, sy, r.width, r.height, 0, 0]));

    const isLight = isLightTheme();
    const bg = isLight ? { r: 0.969, g: 0.965, b: 0.953, a: 1 } : { r: 0.063, g: 0.063, b: 0.067, a: 1 };
    const encCmd = gpu.dev.createCommandEncoder();
    const pass = encCmd.beginRenderPass({
      colorAttachments: [{ view: gpu.ctx.getCurrentTexture().createView(), clearValue: bg, loadOp: "clear", storeOp: "store" }]
    });
    if (eLen && gpu.edgeBg) { pass.setPipeline(gpu.edgePipe); pass.setBindGroup(0, gpu.edgeBg); pass.draw(6, eLen / 8); }
    if (nLen && gpu.nodeBg) { pass.setPipeline(gpu.nodePipe); pass.setBindGroup(0, gpu.nodeBg); pass.draw(6, nLen / 8); }
    pass.end();
    gpu.dev.queue.submit([encCmd.finish()]);

    // Crisp HiDPI 2D label overlay directly from WASM JSON
    const lctx = lCanvas.getContext("2d");
    lctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    lctx.clearRect(0, 0, r.width, r.height);
    const lLen = W.explorer_graph_labels_len(H);
    if (lLen) {
      const labels = JSON.parse(dec.decode(new Uint8Array(W.memory.buffer, W.explorer_graph_labels_ptr(H), lLen)));
      lctx.font = '500 11px -apple-system, BlinkMacSystemFont, "Inter", system-ui, sans-serif';
      lctx.textAlign = "center";
      lctx.textBaseline = "top";
      for (const [, wx, wy, name, flags] of labels) {
        const px = (wx - gpu.cam.cx) * gpu.cam.zoom + r.width * 0.5;
        const py = (wy - gpu.cam.cy) * gpu.cam.zoom + r.height * 0.5 + 12;
        if (px < -80 || px > r.width + 80 || py < -20 || py > r.height + 20) continue;
        lctx.fillStyle = (flags & 1) ? (isLight ? "#d63c12" : "#ebeae6")
          : (flags & 4) ? (isLight ? "rgba(22,22,23,0.22)" : "rgba(235,234,230,0.20)")
          : (isLight ? "rgba(22,22,23,0.72)" : "rgba(235,234,230,0.70)");
        lctx.fillText(name, px, py);
      }
    }
    gpu.raf = requestAnimationFrame(frame);
  };
  gpu.raf = requestAnimationFrame(frame);
}

function isLightTheme() {
  const cur = document.documentElement.getAttribute("data-theme");
  const prefers = window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches;
  return cur ? cur === "light" : prefers;
}

// --- Render cycle (100% HTML generated in Rust/WASM!) ---
function renderAll() {
  if (!W) return;
  statusObj = JSON.parse(wasmReadOut(W.explorer_render_status(H)) || "{}");

  $("tabArchCnt").textContent = statusObj.archetypes ?? 0;
  $("tabCompCnt").textContent = statusObj.components ?? 0;
  $("btnBuiltins").hidden = !statusObj.hasBuiltins;
  $("btnBuiltins").classList.toggle("on", !!statusObj.showBuiltins);
  document.querySelectorAll("#tabs .tab").forEach(t => t.classList.toggle("active", t.dataset.view === statusObj.view));

  const bar = $("pageFind");
  bar.hidden = !statusObj.findOpen;
  const q = $("findInput").value.trim();
  $("findPrev").disabled = !statusObj.findTotal;
  $("findNext").disabled = !statusObj.findTotal;
  if (!q) { $("findCount").textContent = ""; bar.removeAttribute("data-none"); }
  else if (statusObj.findNone) { $("findCount").textContent = "0 of 0"; bar.setAttribute("data-none", ""); }
  else { $("findCount").textContent = `${statusObj.findCur} of ${statusObj.findTotal}`; bar.removeAttribute("data-none"); }

  $("sideRoot").innerHTML = wasmReadOut(W.explorer_render_sidebar(H));
  bindGlideHover();

  const isGraph = statusObj.view === "graph";
  $("contentArea").classList.toggle("fullbleed", isGraph);
  if (isGraph && gpu.mountedCanvas && $("gpuCanvas") === gpu.mountedCanvas) {
    // Keep live WebGPU canvas intact; only update HUD if needed
    const tmp = document.createElement("div");
    tmp.innerHTML = wasmReadOut(W.explorer_render_main(H));
    const newBl = tmp.querySelector(".gpu-hud-bl");
    const oldBl = $("viewRoot").querySelector(".gpu-hud-bl");
    if (newBl && oldBl) oldBl.replaceWith(newBl);
    else if (newBl) $("gpuStage").appendChild(newBl);
    const newTl = tmp.querySelector(".gpu-hud-tl");
    const oldTl = $("viewRoot").querySelector(".gpu-hud-tl");
    if (newTl && oldTl) oldTl.replaceWith(newTl);
  } else {
    stopGpuLoop();
    $("viewRoot").innerHTML = wasmReadOut(W.explorer_render_main(H));
    if (isGraph) mountWebGpu();
  }

  if (statusObj.findOpen && statusObj.findTotal) {
    const curRow = document.querySelector(`.row[data-slot="${statusObj.selected}"]`);
    if (curRow) curRow.scrollIntoView({ block: "nearest" });
    const curMark = $("viewRoot").querySelector("mark.fm.cur");
    if (curMark) curMark.scrollIntoView({ block: "center" });
  }
}

function bindGlideHover() {
  const list = $("navList"), hov = $("glideHov");
  if (!list || !hov) return;
  list.addEventListener("pointermove", (ev) => {
    const row = ev.target.closest(".row");
    if (!row || row.getAttribute("aria-current") === "page") { hov.style.opacity = "0"; return; }
    const lr = list.getBoundingClientRect(), rr = row.getBoundingClientRect();
    hov.style.transform = `translate3d(${rr.left - lr.left}px, ${rr.top - lr.top}px, 0)`;
    hov.style.width = `${rr.width}px`;
    hov.style.height = `${rr.height}px`;
    hov.style.opacity = "1";
  });
  list.addEventListener("pointerleave", () => { hov.style.opacity = "0"; });
}

async function refresh() {
  const r = await api("GET", "/api/world");
  if (!r.ok || !r.text) return;
  worldJson = r.json;
  wasmCallBytes(W.explorer_load_world, r.text);
  renderAll();
}

async function commitField(inp) {
  const slot = statusObj.selected;
  const cname = inp.dataset.comp;
  const ftype = inp.dataset.type || "";
  const st = $(`st_${cname}`);
  const well = inp.closest(".well");
  let val;
  if (inp.type === "checkbox") {
    val = inp.checked;
    const lbl = inp.parentElement && inp.parentElement.querySelector(".sw-val");
    if (lbl) lbl.textContent = val ? "true" : "false";
    wasmCallBytes(W.explorer_action, JSON.stringify({ op: "patch_local", slot, comp: cname, field: inp.dataset.field, bool: val }));
  } else if (/^(f32|f64)$/.test(ftype)) {
    val = Number(inp.value);
    wasmCallBytes(W.explorer_action, JSON.stringify({ op: "patch_local", slot, comp: cname, field: inp.dataset.field, num: val }));
  } else if (/^(i|u)(8|16|32|64|size)$/.test(ftype)) {
    val = Math.trunc(Number(inp.value));
    wasmCallBytes(W.explorer_action, JSON.stringify({ op: "patch_local", slot, comp: cname, field: inp.dataset.field, num: val }));
  } else {
    val = inp.value;
  }
  const payload = inp.dataset.direct ? val : { [inp.dataset.field]: val };
  const r = await api("PATCH", `/api/entity/${slot}/component/${encodeURIComponent(cname)}`, payload);
  if (r.ok) {
    if (st) { st.className = "comp-status ok"; st.textContent = "saved"; }
    if (well) { well.classList.remove("err"); well.classList.add("saved"); setTimeout(() => well.classList.remove("saved"), 550); }
    setTimeout(() => { if (st && st.textContent === "saved") st.textContent = ""; }, 1100);
  } else {
    if (well) well.classList.add("err");
    if (st) { st.className = "comp-status err"; st.textContent = (r.json && r.json.error) || `err ${r.status}`; }
  }
}

// --- Delegated events (zero per-render listener churn!) ---
$("sideRoot").addEventListener("click", async (ev) => {
  const t = ev.target;
  if (t.closest("#btnOpenFind")) { dispatch({ op: "find_open" }); $("findInput").focus(); $("findInput").select(); return; }
  if (t.closest("#btnSpawn")) { openSpawnModal(); return; }
  if (t.closest("#btnJson")) { openExport("json"); return; }
  if (t.closest("#btnTab")) { openExport("tab"); return; }
  if (t.closest("#btnTheme")) {
    const nextLight = !isLightTheme();
    document.documentElement.setAttribute("data-theme", nextLight ? "light" : "dark");
    dispatch({ op: "theme", light: nextLight });
    return;
  }
  const inst = t.closest("[data-inst]");
  if (inst) {
    const pSlot = Number(inst.dataset.inst);
    const p = worldJson && worldJson.entities.find(e => e.slot === pSlot);
    const name = `${p && p.name ? p.name : "Entity"}_${(worldJson ? worldJson.stats.entities : 0) + 1}`;
    const r = await api("POST", "/api/entity", { name, prefab: pSlot });
    if (r.ok && r.json) { await refresh(); dispatch({ op: "select", slot: r.json.slot, openInspector: true }); }
    return;
  }
  const tog = t.closest("[data-toggle]");
  if (tog && !tog.classList.contains("leaf")) { dispatch({ op: "toggle_tree", slot: Number(tog.dataset.toggle) }); return; }
  const row = t.closest(".row[data-slot]");
  if (row) dispatch({ op: "select", slot: Number(row.dataset.slot), openInspector: statusObj.view !== "graph" });
});

$("viewRoot").addEventListener("click", async (ev) => {
  const t = ev.target;
  const jmp = t.closest("[data-jump]");
  if (jmp) { dispatch({ op: "select", slot: Number(jmp.dataset.jump), openInspector: true }); return; }
  const qrow = t.closest("[data-qslot]");
  if (qrow) { dispatch({ op: "select", slot: Number(qrow.dataset.qslot), openInspector: true }); return; }
  if (t.closest("[data-open-graph]")) { dispatch({ op: "tab", view: "graph" }); return; }
  if (t.closest("[data-open-insp]")) { dispatch({ op: "tab", view: "inspector" }); return; }
  const gmode = t.closest("[data-gmode]");
  if (gmode) { gpu.didFit = false; dispatch({ op: "graph_mode", mode: gmode.dataset.gmode }); fitCamera(); return; }
  if (t.closest("#btnGpuFit")) { fitCamera(); return; }
  if (t.closest("#btnToggleAllComps")) { dispatch({ op: "toggle_all_comps" }); return; }
  const copy = t.closest("[data-copy-comp]");
  if (copy) {
    ev.preventDefault(); ev.stopPropagation();
    const cn = copy.dataset.copyComp;
    const ent = worldJson && worldJson.entities.find(e => e.slot === statusObj.selected);
    if (ent && navigator.clipboard) navigator.clipboard.writeText(JSON.stringify(ent.components[cn], null, 2));
    const st = $(`st_${cn}`);
    if (st) { st.className = "comp-status ok"; st.textContent = "copied"; setTimeout(() => { if (st.textContent === "copied") st.textContent = ""; }, 900); }
    return;
  }
  const rmTag = t.closest("[data-rm-tag]");
  if (rmTag) { await api("DELETE", `/api/entity/${statusObj.selected}/tag/${encodeURIComponent(rmTag.dataset.rmTag)}`); await refresh(); return; }
  if (t.closest("#btnDeleteEnt")) { await api("DELETE", `/api/entity/${statusObj.selected}`); await refresh(); return; }
  if (t.closest("#btnAttach")) {
    const cname = $("attachSel").value;
    const meta = worldJson && worldJson.components.find(c => c.name === cname);
    let def = 0;
    if (meta && meta.schema && meta.schema.kind === "struct") {
      def = {};
      for (const f of meta.schema.fields || []) def[f.name] = f.type === "bool" ? false : 0;
    }
    const r = await api("PUT", `/api/entity/${statusObj.selected}/component/${encodeURIComponent(cname)}`, def);
    if (!r.ok) alert((r.json && r.json.error) || `HTTP ${r.status}`);
    await refresh();
    return;
  }
  if (t.closest("#btnRunQ")) { runQuery(); return; }
});

$("viewRoot").addEventListener("change", async (ev) => {
  const t = ev.target;
  if (t.id === "selParent") {
    const p = t.value;
    const r = p === "" ? await api("DELETE", `/api/entity/${statusObj.selected}/parent`) : await api("POST", `/api/entity/${statusObj.selected}/parent/${p}`);
    if (!r.ok) alert((r.json && r.json.error) || `HTTP ${r.status}`);
    await refresh();
    return;
  }
  if (t.matches("[data-comp]")) commitField(t);
});

$("viewRoot").addEventListener("keydown", async (ev) => {
  const t = ev.target;
  if (ev.key !== "Enter") return;
  if (t.id === "inTag") {
    const v = t.value.trim();
    if (v) { await api("POST", `/api/entity/${statusObj.selected}/tag/${encodeURIComponent(v)}`); await refresh(); }
  } else if (t.id === "qWith" || t.id === "qWithout") {
    runQuery();
  } else if (t.matches("[data-comp]")) {
    ev.preventDefault(); t.blur();
  }
});

// Track <details> collapse state in WASM ECS
$("viewRoot").addEventListener("toggle", (ev) => {
  const det = ev.target;
  if (det.matches && det.matches(".comp-card")) {
    wasmCallBytes(W.explorer_action, JSON.stringify({ op: "toggle_comp", name: det.dataset.cname, open: det.open }));
  }
}, true);

// Drag-to-scrub numeric labels (Shift = 0.1x, Alt = 10x)
$("viewRoot").addEventListener("pointerdown", (ev) => {
  const lbl = ev.target.closest("[data-scrub]");
  if (!lbl || ev.button !== 0) return;
  const well = lbl.closest(".well");
  const inp = well && well.querySelector("input");
  if (!inp) return;
  ev.preventDefault();
  lbl.setPointerCapture(ev.pointerId);
  const startX = ev.clientX, startVal = Number(inp.value) || 0;
  const isInt = /^(i|u)(8|16|32|64|size)$/.test(inp.dataset.type || "");
  let moved = false, lastPatch = 0;
  well.classList.add("scrubbing");
  const onMove = (mv) => {
    const dx = mv.clientX - startX;
    if (!moved && Math.abs(dx) < 3) return;
    moved = true;
    const mult = mv.shiftKey ? 0.1 : (mv.altKey ? 10 : 1);
    let next = startVal + Math.round(dx / 3) * (isInt ? 1 : 0.5) * mult;
    next = isInt ? Math.round(next) : Math.round(next * 100) / 100;
    if (String(next) !== inp.value) {
      inp.value = String(next);
      const now = Date.now();
      if (now - lastPatch > 80) { lastPatch = now; commitField(inp); }
    }
  };
  const onUp = () => {
    lbl.removeEventListener("pointermove", onMove);
    lbl.removeEventListener("pointerup", onUp);
    lbl.removeEventListener("pointercancel", onUp);
    well.classList.remove("scrubbing");
    if (moved) commitField(inp); else { inp.focus(); inp.select(); }
  };
  lbl.addEventListener("pointermove", onMove);
  lbl.addEventListener("pointerup", onUp);
  lbl.addEventListener("pointercancel", onUp);
});

async function runQuery() {
  const w = ($("qWith") && $("qWith").value.trim()) || "";
  const wo = ($("qWithout") && $("qWithout").value.trim()) || "";
  const qs = [];
  if (w) qs.push("with=" + encodeURIComponent(w));
  if (wo) qs.push("without=" + encodeURIComponent(wo));
  const r = await api("GET", "/api/query" + (qs.length ? "?" + qs.join("&") : ""));
  if (r.ok) dispatch({ op: "query_result", with: w, without: wo, raw: r.text });
}

// Topbar & Find bar
$("tabs").addEventListener("click", (ev) => {
  const t = ev.target.closest(".tab");
  if (t) dispatch({ op: "tab", view: t.dataset.view });
});
$("btnRefresh").addEventListener("click", refresh);
$("btnBuiltins").addEventListener("click", () => dispatch({ op: "builtins" }));
$("btnAuto").addEventListener("click", () => {
  if (autoTimer) { clearInterval(autoTimer); autoTimer = null; $("btnAuto").classList.remove("on"); }
  else { autoTimer = setInterval(refresh, 1500); $("btnAuto").classList.add("on"); }
});

$("findClose").addEventListener("click", () => dispatch({ op: "find_close" }));
$("findPrev").addEventListener("click", () => dispatch({ op: "find_step", delta: -1 }));
$("findNext").addEventListener("click", () => dispatch({ op: "find_step", delta: 1 }));
$("findInput").addEventListener("input", () => dispatch({ op: "find_query", q: $("findInput").value }));
$("findInput").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") { ev.preventDefault(); dispatch({ op: "find_step", delta: ev.shiftKey ? -1 : 1 }); }
  else if (ev.key === "Escape") { ev.preventDefault(); dispatch({ op: "find_close" }); }
});
window.addEventListener("keydown", (ev) => {
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "f") {
    ev.preventDefault(); dispatch({ op: "find_open" }); $("findInput").focus(); $("findInput").select();
  } else if (ev.key === "Escape" && statusObj.findOpen) {
    dispatch({ op: "find_close" });
  } else if ((ev.key === "f" || ev.key === "F") && statusObj.view === "graph" && document.activeElement.tagName !== "INPUT") {
    fitCamera();
  }
});

// Modals
async function openExport(fmt) {
  const r = await api("GET", fmt === "tab" ? "/api/world.tab" : "/api/world");
  $("exportTitle").textContent = fmt === "tab" ? "World Snapshot (TAB)" : "World Snapshot (JSON)";
  $("exportPre").textContent = fmt === "json" && r.json ? JSON.stringify(r.json, null, 2) : r.text;
  $("exportModal").classList.add("open");
}
$("btnCloseExport").addEventListener("click", () => $("exportModal").classList.remove("open"));
$("btnDoneExport").addEventListener("click", () => $("exportModal").classList.remove("open"));
$("btnCopyExport").addEventListener("click", () => navigator.clipboard && navigator.clipboard.writeText($("exportPre").textContent));

function openSpawnModal() {
  if (!worldJson) return;
  $("spErr").textContent = ""; $("spName").value = ""; $("spTags").value = "";
  $("spParent").innerHTML = `<option value="">Root</option>` +
    worldJson.entities.map(e => `<option value="${e.slot}" ${e.slot === statusObj.selected ? "selected" : ""}>${e.name || ("entity_" + e.slot)}</option>`).join("");
  $("spPrefab").innerHTML = `<option value="">None</option>` +
    worldJson.prefabs.map(p => `<option value="${p.slot}">${p.name || ("entity_" + p.slot)}</option>`).join("");
  $("spawnModal").classList.add("open"); $("spName").focus();
}
$("btnCloseSpawn").addEventListener("click", () => $("spawnModal").classList.remove("open"));
$("btnCancelSpawn").addEventListener("click", () => $("spawnModal").classList.remove("open"));
$("btnConfirmSpawn").addEventListener("click", async () => {
  const payload = {};
  if ($("spName").value.trim()) payload.name = $("spName").value.trim();
  if ($("spParent").value !== "") payload.parent = Number($("spParent").value);
  if ($("spPrefab").value !== "") payload.prefab = Number($("spPrefab").value);
  if ($("spTags").value.trim()) payload.tags = $("spTags").value.split(",").map(s => s.trim()).filter(Boolean);
  const r = await api("POST", "/api/entity", payload);
  if (!r.ok) { $("spErr").textContent = (r.json && r.json.error) || `HTTP ${r.status}`; return; }
  $("spawnModal").classList.remove("open");
  await refresh();
  if (r.json) dispatch({ op: "select", slot: r.json.slot, openInspector: true });
});

// --- Boot WASM relational ECS ---
(async () => {
  const resp = await fetch("/explorer.wasm");
  if (!resp.ok) {
    $("viewRoot").innerHTML = `<div class="empty">Missing /explorer.wasm (HTTP ${resp.status}). Build blaze-ecs-wasm for wasm32-unknown-unknown.</div>`;
    return;
  }
  const bytes = await resp.arrayBuffer();
  const { instance } = await WebAssembly.instantiate(bytes, {});
  W = instance.exports;
  H = W.explorer_new();
  wasmCallBytes(W.explorer_action, JSON.stringify({ op: "theme", light: isLightTheme() }));
  await refresh();
})();
</script>
</body>
</html>
"##;
