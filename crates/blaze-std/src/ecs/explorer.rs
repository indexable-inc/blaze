//! Pure sans-IO Flecs Explorer HTTP/JSON/TAB API and embedded interactive Web UI
//! (`#![no_std]`, `#![forbid(unsafe_code)]`, zero sockets, zero ambient I/O).
//!
//! Callers supply raw HTTP/1.1 bytes via [`handle_http_request`] (or pre-parsed `(method, path,
//! body)` via [`handle_request`]) and receive deterministic response bytes backed by their
//! explicit allocator `A`.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_own::{AllocFailure, Erased, String as OwnString, Vec as OwnVec};

use crate::data::{Int, Map, Value};
use crate::json::{Limits as JsonLimits, WriteOptions as JsonWriteOptions};

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

/// Handle a single Explorer API or UI request against `world` with zero ambient I/O.
pub fn handle_request<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    method: &str,
    path: &str,
    body: &[u8],
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
        let tab_result = crate::tab::write_pretty(&val, erased.handle());
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
            let patch_val = match crate::json::read_in(body, JsonLimits::SHIPPED, alloc) {
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
        match crate::json::read_in(trimmed, JsonLimits::SHIPPED, alloc) {
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
    match crate::json::to_string_in(val, JsonWriteOptions::COMPACT, alloc) {
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
    match crate::json::to_string_in(&Value::Map(map), JsonWriteOptions::COMPACT, alloc) {
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

  --g-0: #0c0c0d;
  --g-1: #101011;
  --g-2: #141415;
  --g-3: #19191a;
  --g-4: #1f1f20;
  --g-5: #28282a;
  --g-6: #333335;
  --g-7: #414143;
  --g-8: #57575a;
  --g-9: #727274;
  --g-10: #929190;
  --g-11: #b3b2af;
  --g-12: #d4d3cf;
  --g-13: #ebeae6;

  --bg: var(--g-1);
  --bg-2: var(--g-2);
  --bg-3: var(--g-3);
  --bg-4: var(--g-4);
  --line: var(--g-5);
  --line-2: var(--g-6);
  --ink-1: var(--g-13);
  --ink-2: var(--g-11);
  --ink-3: #878684;
  --ink-4: var(--g-8);

  --code-bg: color-mix(in srgb, var(--g-2) 70%, var(--g-1));
  --s-tint: color-mix(in srgb, var(--ink-1) 6%, transparent);
  --s-tint-soft: color-mix(in srgb, var(--ink-1) 3.5%, transparent);

  --danger: #ff7168;
  --ok: #4cc27a;
  --fire: #e0461a;

  --syn-text: #bcbec4;
  --syn-kw: #cf8e6d;
  --syn-fn: #56a8f5;
  --syn-ty: #c77dbb;
  --syn-mark: #7a7e85;

  --side-w: 268px;
  --font-sans: 'Inter', -apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Helvetica Neue', 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'Berkeley Mono', ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}

@media (prefers-color-scheme: light) {
  :root:not([data-theme="dark"]) {
    --g-12: #2a2a2c;
    --bg: #f2f1ee;
    --bg-2: #eae9e4;
    --bg-3: #f6f5f2;
    --bg-4: #e2e1dc;
    --line: #dcdbd5;
    --line-2: #cdccc6;
    --ink-1: #161617;
    --ink-2: #46464a;
    --ink-3: #6b6a6e;
    --ink-4: #a2a19d;
    --code-bg: #ebeae5;
    --danger: #c4332a;
    --ok: #2b8a4f;
    --fire: #d63c12;
    --syn-text: #080808;
    --syn-kw: #0033b3;
    --syn-fn: #00627a;
    --syn-ty: #871094;
    --syn-mark: #8c8c8c;
  }
}

:root[data-theme="light"] {
  --g-12: #2a2a2c;
  --bg: #f2f1ee;
  --bg-2: #eae9e4;
  --bg-3: #f6f5f2;
  --bg-4: #e2e1dc;
  --line: #dcdbd5;
  --line-2: #cdccc6;
  --ink-1: #161617;
  --ink-2: #46464a;
  --ink-3: #6b6a6e;
  --ink-4: #a2a19d;
  --code-bg: #ebeae5;
  --danger: #c4332a;
  --ok: #2b8a4f;
  --fire: #d63c12;
  --syn-text: #080808;
  --syn-kw: #0033b3;
  --syn-fn: #00627a;
  --syn-ty: #871094;
  --syn-mark: #8c8c8c;
}

* { box-sizing: border-box; }
html {
  background: var(--bg);
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
  font-synthesis: none;
}
@media (pointer: fine) { html { overscroll-behavior-y: none; } }
body {
  margin: 0;
  font-family: var(--font-sans);
  font-size: 14px;
  line-height: 1.55;
  letter-spacing: -0.011em;
  color: var(--ink-2);
  background: var(--bg);
  text-rendering: optimizeLegibility;
}
::selection { background: color-mix(in srgb, var(--ink-1) 20%, transparent); color: var(--ink-1); }
:focus-visible { outline: 1.5px solid var(--ink-3); outline-offset: 2px; border-radius: 4px; }
code, pre, .mono { font-family: var(--font-mono); letter-spacing: 0; }
button, input, select, textarea { font: inherit; color: inherit; }

/* Blaze shell (.frame: sticky sidebar + rounded pane) */
.frame {
  display: grid;
  grid-template-columns: var(--side-w) minmax(0, 1fr);
  gap: 4px;
  min-height: 100svh;
  padding: 6px clamp(8px, 1vw, 14px) 10px 0;
}

/* Left sidebar */
.side {
  position: sticky;
  top: 0;
  display: flex;
  flex-direction: column;
  height: 100svh;
  margin-block: -6px -10px;
  padding: 14px 0 0 clamp(10px, 1.1vw, 16px);
}
.sidetop {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin: 0 8px 12px 0;
}
.brand {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  padding: 4px 6px;
  border-radius: 8px;
  color: var(--ink-1);
  text-decoration: none;
  font-weight: 600;
  font-size: 15px;
  letter-spacing: -0.025em;
  user-select: none;
}
.ember {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--fire);
  flex: none;
}
.brand .sub {
  font-family: var(--font-mono);
  font-size: 11px;
  font-weight: 400;
  color: var(--ink-3);
  letter-spacing: 0;
}
.iconbtn {
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  padding: 0;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: var(--ink-3);
  cursor: pointer;
  transition: background-color 0.12s, color 0.12s;
}
.iconbtn:hover { background: var(--s-tint); color: var(--ink-1); }

.searchbtn {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
  height: 34px;
  margin: 0 8px 10px 0;
  padding: 0 8px 0 11px;
  border: 0;
  border-radius: 999px;
  background: var(--s-tint);
  color: var(--ink-3);
  font-size: 13px;
  cursor: pointer;
  transition: background-color 0.15s, color 0.15s;
}
.searchbtn:hover { background: color-mix(in srgb, var(--ink-1) 9%, transparent); color: var(--ink-1); }
.searchbtn span { flex: 1; text-align: left; }
.searchbtn kbd {
  padding: 1px 6px;
  border-radius: 6px;
  box-shadow: inset 0 0 0 1px var(--line-2);
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--ink-3);
}

.scrollbox {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overscroll-behavior: contain;
  padding-right: 8px;
  scrollbar-width: none;
  mask-image: linear-gradient(to bottom, #000 calc(100% - 28px), transparent);
  -webkit-mask-image: linear-gradient(to bottom, #000 calc(100% - 28px), transparent);
}
.scrollbox::-webkit-scrollbar { display: none; }
.navlist { position: relative; padding-bottom: 28px; }
.glide-hov {
  position: absolute;
  top: 0;
  left: 0;
  z-index: 0;
  border-radius: 8px;
  pointer-events: none;
  background: var(--s-tint-soft);
  opacity: 0;
  transition: transform 0.14s cubic-bezier(0.34, 1.3, 0.5, 1), width 0.14s cubic-bezier(0.34, 1.3, 0.5, 1), height 0.1s, opacity 0.1s;
}

.sec + .sec { margin-top: 12px; }
.sec > summary { list-style: none; }
.sec > summary::-webkit-details-marker { display: none; }
.sechead {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  height: 28px;
  padding: 0 8px;
  border-radius: 8px;
  font-size: 12px;
  font-weight: 500;
  color: var(--ink-3);
  cursor: pointer;
  user-select: none;
  transition: color 0.12s;
}
.sechead:hover { color: var(--ink-1); }
.sechead span { flex: 1; }
.sechead .cnt { flex: none; font-family: var(--font-mono); font-size: 11px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.sechead .chev { width: 12px; height: 12px; transition: transform 0.18s; }
.sec:not([open]) > .sechead .chev { transform: rotate(-90deg); }

.row {
  position: relative;
  z-index: 1;
  display: flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  padding: 0 8px 0 10px;
  border-radius: 8px;
  color: var(--ink-2);
  font-size: 13.5px;
  cursor: pointer;
  user-select: none;
  transition: color 0.12s, background-color 0.12s;
}
.row:hover { color: var(--ink-1); }
.row[aria-current="page"] { color: var(--ink-1); font-weight: 500; background: var(--s-tint); }
.row .label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.row .num { flex: none; font-family: var(--font-mono); font-size: 11px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.row[aria-current="page"] .num { color: var(--ink-3); }
.row .pdot { width: 5px; height: 5px; border-radius: 50%; background: var(--fire); flex: none; opacity: 0.85; }
.row .twisty {
  display: grid;
  place-items: center;
  width: 16px;
  height: 16px;
  margin-left: -4px;
  color: var(--ink-4);
  flex: none;
  transition: transform 0.15s, color 0.12s;
}
.row .twisty:hover { color: var(--ink-1); }
.row .twisty.shut { transform: rotate(-90deg); }
.row .twisty.leaf { visibility: hidden; }
.row .inst {
  opacity: 0;
  padding: 2px 7px;
  border: 1px solid var(--line-2);
  border-radius: 999px;
  background: transparent;
  color: var(--ink-2);
  font-size: 11px;
  cursor: pointer;
  transition: opacity 0.12s, color 0.12s, border-color 0.12s, background-color 0.12s;
}
.row:hover .inst, .row:focus-within .inst { opacity: 1; }
.row .inst:hover { color: var(--ink-1); border-color: var(--ink-4); background: var(--s-tint); }

.kids {
  margin: 1px 0 3px 15px;
  padding-left: 6px;
  border-left: 1px solid var(--line-2);
}

.foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4px;
  flex: none;
  padding: 8px 8px 10px 4px;
  border-top: 1px solid var(--line);
}
.foot-meta {
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--ink-4);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.foot-acts { display: flex; align-items: center; gap: 2px; }
.foot button {
  display: inline-flex;
  align-items: center;
  height: 24px;
  padding: 0 7px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-3);
  font-family: var(--font-mono);
  font-size: 11px;
  cursor: pointer;
  transition: background-color 0.12s, color 0.12s;
}
.foot button:hover { background: var(--s-tint); color: var(--ink-1); }

/* Right rounded pane (exact shell.css .pane) */
.pane {
  position: relative;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: calc(100svh - 16px);
  border-radius: 16px;
  box-shadow: inset 0 0 0 1px var(--line-2);
  background: var(--bg);
  overflow: hidden;
}

/* Minimal top bar inside pane */
.topbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  height: 46px;
  padding: 0 clamp(20px, 3vw, 40px);
  border-bottom: 1px solid var(--line);
  flex: none;
}
.tabs { display: flex; align-items: center; gap: 4px; height: 100%; }
.tab {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 100%;
  padding: 0 10px;
  border: 0;
  background: transparent;
  color: var(--ink-3);
  font-size: 13px;
  font-weight: 500;
  cursor: pointer;
  transition: color 0.12s;
}
.tab:hover { color: var(--ink-1); }
.tab.active { color: var(--ink-1); }
.tab.active::after {
  content: '';
  position: absolute;
  left: 10px;
  right: 10px;
  bottom: -1px;
  height: 1.5px;
  background: var(--ink-1);
}
.tab .tcnt { font-family: var(--font-mono); font-size: 11px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.tab.active .tcnt { color: var(--ink-3); }

.top-acts { display: flex; align-items: center; gap: 6px; }
.ghost {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 28px;
  padding: 0 9px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--ink-3);
  font-size: 12.5px;
  cursor: pointer;
  transition: background-color 0.12s, color 0.12s;
}
.ghost:hover { background: var(--s-tint); color: var(--ink-1); }
.ghost[hidden] { display: none; }
.ghost.on { color: var(--ink-1); background: var(--s-tint); }
.ghost .live-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--ink-4); }
.ghost.on .live-dot { background: var(--ok); }
.ghost.danger:hover { color: var(--danger); background: color-mix(in srgb, var(--danger) 10%, transparent); }

/* Main reading column inside pane */
.content {
  flex: 1;
  overflow-y: auto;
  padding: 0 clamp(20px, 3.5vw, 48px) 72px;
}
.col {
  width: 100%;
  max-width: 780px;
  margin: 0 auto;
}

/* Editorial entity header (shell.css .head) */
.head { padding: 32px 0 22px; }
.crumb {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  margin: 0 0 10px;
  font-size: 12.5px;
  color: var(--ink-3);
}
.crumb a { color: inherit; text-decoration: none; cursor: pointer; }
.crumb a:hover { color: var(--ink-1); }
.crumb .sep { color: var(--ink-4); }
.head-row {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
}
.head h1 {
  margin: 0;
  font-size: clamp(24px, 2.8vw, 32px);
  line-height: 1.15;
  font-weight: 600;
  letter-spacing: -0.03em;
  color: var(--ink-1);
}
.head-sub {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 10px;
  margin-top: 8px;
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--ink-3);
  font-variant-numeric: tabular-nums;
}
.badge {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 2px 9px;
  border: 1px solid var(--line-2);
  border-radius: 999px;
  font-family: var(--font-sans);
  font-size: 11.5px;
  font-weight: 500;
  color: var(--ink-2);
}
.badge.fire { border-color: color-mix(in srgb, var(--fire) 45%, var(--line-2)); color: var(--fire); }
.badge a { color: inherit; text-decoration: none; cursor: pointer; }
.badge a:hover { text-decoration: underline; }
.badge button {
  padding: 0;
  border: 0;
  background: none;
  color: var(--ink-4);
  cursor: pointer;
  line-height: 1;
  font-size: 13px;
}
.badge button:hover { color: var(--danger); }

/* Quiet hairline metadata strip */
.meta-strip {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 14px 20px;
  padding: 12px 0;
  border-top: 1px solid var(--line);
  font-size: 13px;
}
.meta-group { display: inline-flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.meta-label { font-size: 12px; color: var(--ink-3); }
.quiet-select {
  height: 26px;
  padding: 0 22px 0 8px;
  border: 1px solid var(--line-2);
  border-radius: 7px;
  background: var(--bg-2);
  color: var(--ink-1);
  font-size: 12.5px;
  cursor: pointer;
}
.quiet-select:hover { border-color: var(--ink-4); }
.inline-add {
  width: 92px;
  height: 24px;
  padding: 0 8px;
  border: 1px dashed var(--line-2);
  border-radius: 999px;
  background: transparent;
  color: var(--ink-1);
  font-size: 12px;
  outline: none;
  transition: border-color 0.12s, width 0.15s;
}
.inline-add::placeholder { color: var(--ink-4); }
.inline-add:focus { width: 136px; border-style: solid; border-color: var(--ink-3); background: var(--bg-2); }

/* Components as shell.css .goal hairline sections */
.section-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 20px 0 10px;
}
.section-bar h2 {
  margin: 0;
  font-size: 12px;
  font-weight: 500;
  color: var(--ink-3);
  letter-spacing: 0.01em;
}
.comp-sec { border-top: 1px solid var(--line); }
.comp-sec:last-of-type { border-bottom: 1px solid var(--line); }
.comp-sec > summary {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 13px 2px;
  list-style: none;
  cursor: pointer;
  user-select: none;
}
.comp-sec > summary::-webkit-details-marker { display: none; }
.comp-name {
  font-size: 15px;
  font-weight: 600;
  letter-spacing: -0.015em;
  color: var(--ink-1);
}
.comp-size { font-family: var(--font-mono); font-size: 11.5px; color: var(--ink-4); font-variant-numeric: tabular-nums; }
.comp-status {
  margin-left: auto;
  font-family: var(--font-mono);
  font-size: 11.5px;
  color: var(--ink-3);
  min-height: 16px;
}
.comp-status.ok { color: var(--ok); }
.comp-status.err { color: var(--danger); }
.comp-sec .chev { width: 13px; height: 13px; color: var(--ink-4); transition: transform 0.16s; }
.comp-sec:not([open]) .chev { transform: rotate(-90deg); }

.fields { padding: 0 2px 14px; }
.frow {
  display: grid;
  grid-template-columns: 160px minmax(0, 1fr) 60px;
  align-items: center;
  gap: 12px;
  min-height: 34px;
  padding: 2px 0;
}
.frow + .frow { border-top: 1px solid color-mix(in srgb, var(--line) 55%, transparent); }
.fname {
  font-family: var(--font-mono);
  font-size: 12.5px;
  color: var(--ink-2);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ftype {
  font-family: var(--font-mono);
  font-size: 11.5px;
  color: var(--syn-ty);
  text-align: right;
  opacity: 0.85;
}
.finput {
  width: 100%;
  height: 28px;
  padding: 0 8px;
  border: 1px solid transparent;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-1);
  font-family: var(--font-mono);
  font-size: 13px;
  font-variant-numeric: tabular-nums;
  outline: none;
  transition: background-color 0.12s, border-color 0.12s;
}
.finput:hover { background: var(--s-tint-soft); border-color: var(--line); }
.finput:focus { background: var(--bg-2); border-color: var(--fire); }

/* Minimal custom bool switch */
.switch {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  user-select: none;
  padding-left: 8px;
}
.switch input { position: absolute; opacity: 0; pointer-events: none; }
.track {
  position: relative;
  width: 30px;
  height: 17px;
  border-radius: 999px;
  background: var(--line-2);
  transition: background-color 0.15s;
}
.track::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: 13px;
  height: 13px;
  border-radius: 50%;
  background: var(--ink-2);
  transition: transform 0.15s, background-color 0.15s;
}
.switch input:checked + .track { background: var(--fire); }
.switch input:checked + .track::after { transform: translateX(13px); background: #fff; }
.switch-val { font-family: var(--font-mono); font-size: 12.5px; color: var(--ink-1); }

/* Editorial tables (Archetypes & Query) */
.etable {
  width: 100%;
  border-collapse: collapse;
  margin-top: 8px;
  font-size: 13px;
  font-variant-numeric: tabular-nums;
}
.etable th {
  text-align: left;
  padding: 10px 8px;
  border-bottom: 1px solid var(--line-2);
  font-size: 11.5px;
  font-weight: 500;
  color: var(--ink-3);
}
.etable td {
  padding: 10px 8px;
  border-bottom: 1px solid var(--line);
  vertical-align: top;
}
.etable tr:hover td { background: var(--s-tint-soft); }
.sig-list { display: flex; flex-wrap: wrap; gap: 5px; }
.sig-chip {
  padding: 1px 7px;
  border: 1px solid var(--line-2);
  border-radius: 6px;
  font-family: var(--font-mono);
  font-size: 11.5px;
  color: var(--ink-2);
}
.sig-chip.rel { color: var(--ink-3); border-style: dashed; }

/* Query bar */
.qbar {
  display: grid;
  grid-template-columns: 1fr 1fr auto;
  gap: 8px;
  padding: 20px 0 14px;
}
.qinput {
  height: 34px;
  padding: 0 11px;
  border: 1px solid var(--line-2);
  border-radius: 8px;
  background: var(--bg-2);
  color: var(--ink-1);
  font-family: var(--font-mono);
  font-size: 12.5px;
  outline: none;
}
.qinput::placeholder { color: var(--ink-4); font-family: var(--font-sans); }
.qinput:focus { border-color: var(--ink-3); }
.btn-primary {
  height: 34px;
  padding: 0 14px;
  border: 0;
  border-radius: 8px;
  background: var(--ink-1);
  color: var(--bg);
  font-size: 13px;
  font-weight: 500;
  cursor: pointer;
  transition: opacity 0.12s;
}
.btn-primary:hover { opacity: 0.88; }
.btn-subtle {
  height: 28px;
  padding: 0 10px;
  border: 1px solid var(--line-2);
  border-radius: 7px;
  background: transparent;
  color: var(--ink-2);
  font-size: 12.5px;
  cursor: pointer;
  transition: background-color 0.12s, color 0.12s;
}
.btn-subtle:hover { background: var(--s-tint); color: var(--ink-1); }

/* Code-style schemas */
.schema-block {
  padding: 14px 16px;
  margin: 12px 0;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: var(--code-bg);
  font-family: var(--font-mono);
  font-size: 12.5px;
  line-height: 1.65;
  color: var(--syn-text);
}
.schema-block .kw { color: var(--syn-kw); }
.schema-block .ty { color: var(--syn-ty); }
.schema-block .cm { color: var(--syn-mark); }

/* Exact Blaze .page-find floating bar (styles/intro.css:269), tucked below .topbar */
.page-find {
  position: fixed;
  top: 66px;
  right: 22px;
  z-index: 60;
  display: flex;
  align-items: center;
  gap: 2px;
  height: 40px;
  padding: 0 4px 0 10px;
  background: var(--bg-2);
  border: 1px solid var(--line-2);
  border-radius: 10px;
  box-shadow: 0 8px 24px rgb(0 0 0 / 0.35);
  font: 13px/1 var(--font-sans);
  color: var(--ink-1);
  transition: border-color 120ms;
}
.page-find[hidden] { display: none; }
.page-find:focus-within { border-color: var(--ink-4); }
.page-find input, .page-find input:focus, .page-find input:focus-visible {
  width: 196px;
  height: 100%;
  padding: 0 6px;
  border: 0;
  border-radius: 0;
  outline: none;
  box-shadow: none;
  background: transparent;
  color: var(--ink-1);
  font: inherit;
  font-size: 13.5px;
}
.page-find input::placeholder { color: var(--ink-4); }
.page-find-glass { display: flex; color: var(--ink-4); }
.page-find-count {
  min-width: 56px;
  text-align: right;
  padding-right: 6px;
  color: var(--ink-3);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.page-find[data-none] .page-find-count { color: var(--fire); }
.page-find-sep { width: 1px; height: 18px; margin: 0 4px; background: var(--line-2); }
.page-find button {
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  padding: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--ink-2);
  cursor: pointer;
}
.page-find button:hover:not(:disabled) { background: var(--s-tint); color: var(--ink-1); }
.page-find button:disabled { color: var(--ink-4); cursor: default; opacity: 0.6; }

/* Soft orange match plates (matching client/lib/page/draw.ts:537) */
mark.fm {
  background: color-mix(in srgb, var(--fire) 22%, transparent);
  color: inherit;
  border-radius: 3px;
  padding: 0 1px;
}
mark.fm.cur {
  background: color-mix(in srgb, var(--fire) 46%, transparent);
  color: var(--ink-1);
}
.row.find-hit { background: color-mix(in srgb, var(--fire) 12%, transparent); }
.row.find-cur {
  background: color-mix(in srgb, var(--fire) 24%, transparent);
  color: var(--ink-1);
}

/* Modal dialog matching shell.css .search */
.modal-bg {
  position: fixed;
  inset: 0;
  z-index: 70;
  display: none;
  align-items: flex-start;
  justify-content: center;
  padding-top: 11vh;
  background: rgb(0 0 0 / 0.45);
}
.modal-bg.open { display: flex; }
.modal {
  width: min(520px, calc(100vw - 24px));
  max-height: 76vh;
  display: flex;
  flex-direction: column;
  border-radius: 14px;
  background: var(--bg-3);
  color: var(--ink-2);
  box-shadow: 0 0 0 1px var(--line-2), 0 30px 70px -20px rgb(0 0 0 / 0.5);
  overflow: hidden;
}
.modal-hd {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 13px 16px;
  border-bottom: 1px solid var(--line);
  font-weight: 600;
  font-size: 14px;
  color: var(--ink-1);
}
.modal-bd { padding: 14px 16px; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; }
.modal-bd label { display: flex; flex-direction: column; gap: 4px; font-size: 12px; color: var(--ink-3); }
.modal-bd pre {
  margin: 0;
  padding: 12px;
  border-radius: 8px;
  background: var(--code-bg);
  border: 1px solid var(--line);
  font-size: 12px;
  color: var(--ink-1);
  overflow: auto;
  max-height: 52vh;
  white-space: pre-wrap;
}
.modal-ft {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 11px 16px;
  border-top: 1px solid var(--line);
}
.empty { padding: 48px 0; color: var(--ink-3); font-size: 14px; }

@media (max-width: 820px) {
  .frame { grid-template-columns: 1fr; padding: 8px; }
  .side { position: static; height: auto; margin: 0 0 8px; padding: 0; }
  .frow { grid-template-columns: 120px minmax(0, 1fr) 48px; }
}
</style>
</head>
<body>

<div class="frame">
  <aside class="side">
    <div class="sidetop">
      <a class="brand" href="#" id="brandHome" title="Blaze ECS Explorer">
        <span class="ember"></span>
        <span>blaze</span>
        <span class="sub">ecs</span>
      </a>
      <button class="iconbtn" id="btnSpawn" title="Spawn entity">
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M8 3.2v9.6M3.2 8h9.6"/></svg>
      </button>
    </div>

    <button class="searchbtn" id="btnOpenFind" type="button">
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="7" cy="7" r="4.6"/><path d="M10.5 10.5L13.8 13.8"/></svg>
      <span>Find…</span>
      <kbd>⌘F</kbd>
    </button>

    <div class="scrollbox">
      <div class="navlist" id="navList">
        <div class="glide-hov" id="glideHov"></div>

        <details class="sec" open>
          <summary class="sechead">
            <svg class="chev" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5l3 3 3-3"/></svg>
            <span>Hierarchy</span>
            <span class="cnt" id="treeCount">0</span>
          </summary>
          <div id="tree"></div>
        </details>

        <details class="sec" open>
          <summary class="sechead">
            <svg class="chev" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5l3 3 3-3"/></svg>
            <span>Prefabs</span>
            <span class="cnt" id="prefabCount">0</span>
          </summary>
          <div id="prefabs"></div>
        </details>
      </div>
    </div>

    <div class="foot">
      <span class="foot-meta" id="footMeta">—</span>
      <div class="foot-acts">
        <button id="btnJson" type="button" title="Export world JSON">json</button>
        <button id="btnTab" type="button" title="Export world TAB">tab</button>
        <button id="btnTheme" type="button" title="Toggle light / dark">◐</button>
      </div>
    </div>
  </aside>

  <div class="pane">
    <div class="topbar">
      <div class="tabs" id="tabs">
        <button class="tab active" data-view="inspector" type="button">Inspector</button>
        <button class="tab" data-view="archetypes" type="button">Archetypes <span class="tcnt" id="tabArchCnt">0</span></button>
        <button class="tab" data-view="query" type="button">Query</button>
        <button class="tab" data-view="schemas" type="button">Schemas <span class="tcnt" id="tabCompCnt">0</span></button>
      </div>
      <div class="top-acts">
        <button class="ghost" id="btnBuiltins" type="button" hidden>Builtins</button>
        <button class="ghost" id="btnAuto" type="button"><span class="live-dot"></span>Auto</button>
        <button class="ghost" id="btnRefresh" type="button">Refresh</button>
      </div>
    </div>

    <div class="content">
      <div class="col" id="viewRoot"></div>
    </div>
  </div>
</div>

<!-- Floating Cmd+F find bar modeled after /site/styles/intro.css .page-find -->
<div class="page-find" id="pageFind" hidden role="search" aria-label="Find in world">
  <span class="page-find-glass" aria-hidden="true">
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="7" cy="7" r="4.6"/><path d="M10.5 10.5L13.8 13.8"/></svg>
  </span>
  <input id="findInput" type="text" placeholder="Find entity, component, field…" autocomplete="off" spellcheck="false">
  <span class="page-find-count" id="findCount"></span>
  <span class="page-find-sep"></span>
  <button id="findPrev" type="button" title="Previous match (Shift+Enter)" aria-label="Previous match" disabled>
    <svg width="13" height="13" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 7.5L6 4l3.5 3.5"/></svg>
  </button>
  <button id="findNext" type="button" title="Next match (Enter)" aria-label="Next match" disabled>
    <svg width="13" height="13" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 4.5L6 8l3.5-3.5"/></svg>
  </button>
  <button id="findClose" type="button" title="Close (Escape)" aria-label="Close">
    <svg width="13" height="13" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M3 3l6 6M9 3l-6 6"/></svg>
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
      <span id="spErr" style="color:var(--danger);font-size:12px"></span>
    </div>
    <div class="modal-ft">
      <button class="btn-subtle" id="btnCancelSpawn" type="button">Cancel</button>
      <button class="btn-primary" id="btnConfirmSpawn" type="button">Spawn</button>
    </div>
  </div>
</div>

<script>
const BUILTINS = new Set(["ChildOf","IsA","Prefab","Exclusive","Symmetric","Transitive","OnDeleteCascade"]);
let world = null;
let selectedSlot = null;
let activeView = "inspector";
let hideBuiltins = true;
let collapsed = new Set();
let autoTimer = null;
let queryState = { with: "", without: "", result: null };
let findState = { open: false, query: "", matches: [], cur: -1 };

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));

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

function entityBySlot(slot) {
  return world ? world.entities.find(e => e.slot === slot) : null;
}

function labelOf(e) {
  return e.name || `entity_${e.slot}`;
}

function schemaOf(compName) {
  const c = world && world.components.find(x => x.name === compName);
  return c ? c.schema : null;
}

function compMeta(compName) {
  return world ? world.components.find(x => x.name === compName) : null;
}

function highlightText(text, isCurrentTarget) {
  const raw = String(text ?? "");
  const q = findState.open ? findState.query.trim() : "";
  if (!q) return esc(raw);
  const low = raw.toLowerCase();
  const ql = q.toLowerCase();
  let out = "", pos = 0, idx;
  while ((idx = low.indexOf(ql, pos)) !== -1) {
    out += esc(raw.slice(pos, idx));
    out += `<mark class="fm${isCurrentTarget ? " cur" : ""}">${esc(raw.slice(idx, idx + ql.length))}</mark>`;
    pos = idx + ql.length;
  }
  out += esc(raw.slice(pos));
  return out;
}

function recomputeFind(keepCurrent) {
  const q = findState.query.trim().toLowerCase();
  const prevKey = (keepCurrent && findState.cur >= 0 && findState.matches[findState.cur])
    ? findState.matches[findState.cur].key : null;
  findState.matches = [];
  if (q && world) {
    for (const e of world.entities) {
      if (hideBuiltins && e.name && BUILTINS.has(e.name)) continue;
      const lbl = labelOf(e);
      if (lbl.toLowerCase().includes(q) || (`#${e.slot}`).includes(q)) {
        findState.matches.push({ key: `e:${e.slot}`, slot: e.slot, kind: "entity" });
      }
      for (const t of (e.tags || [])) {
        if (t.toLowerCase().includes(q)) {
          findState.matches.push({ key: `t:${e.slot}:${t}`, slot: e.slot, kind: "tag", tag: t });
        }
      }
      for (const [cn, cv] of Object.entries(e.components || {})) {
        if (cn.toLowerCase().includes(q)) {
          findState.matches.push({ key: `c:${e.slot}:${cn}`, slot: e.slot, kind: "comp", comp: cn });
        }
        if (cv && typeof cv === "object") {
          for (const [fk, fv] of Object.entries(cv)) {
            if (fk.toLowerCase().includes(q) || String(fv).toLowerCase().includes(q)) {
              findState.matches.push({ key: `f:${e.slot}:${cn}:${fk}`, slot: e.slot, kind: "field", comp: cn, field: fk });
            }
          }
        }
      }
    }
  }
  if (!findState.matches.length) {
    findState.cur = -1;
  } else if (prevKey) {
    const idx = findState.matches.findIndex(m => m.key === prevKey);
    findState.cur = idx >= 0 ? idx : 0;
  } else {
    const curSlotIdx = findState.matches.findIndex(m => m.slot === selectedSlot);
    findState.cur = curSlotIdx >= 0 ? curSlotIdx : 0;
  }
  updateFindBar();
}

function updateFindBar() {
  const bar = $("pageFind");
  const q = findState.query.trim();
  const n = findState.matches.length;
  $("findPrev").disabled = n === 0;
  $("findNext").disabled = n === 0;
  if (!q) {
    $("findCount").textContent = "";
    bar.removeAttribute("data-none");
  } else if (n === 0) {
    $("findCount").textContent = "0 of 0";
    bar.setAttribute("data-none", "");
  } else {
    $("findCount").textContent = `${findState.cur + 1} of ${n}`;
    bar.removeAttribute("data-none");
  }
}

function revealCurrentMatch() {
  const m = findState.matches[findState.cur];
  if (!m) return;
  // Expand all ancestors of m.slot
  let cur = entityBySlot(m.slot);
  while (cur && cur.parents && cur.parents.length) {
    collapsed.delete(cur.parents[0]);
    cur = entityBySlot(cur.parents[0]);
  }
  selectedSlot = m.slot;
  activeView = "inspector";
  syncTabs();
  renderAll();
  const el = document.querySelector(`.row[data-slot="${m.slot}"]`);
  if (el) el.scrollIntoView({ block: "nearest" });
  const curMark = $("viewRoot").querySelector("mark.fm.cur");
  if (curMark) curMark.scrollIntoView({ block: "center" });
}

function openFind() {
  findState.open = true;
  $("pageFind").hidden = false;
  $("findInput").focus();
  $("findInput").select();
  recomputeFind(true);
  renderAll();
}

function closeFind() {
  findState.open = false;
  $("pageFind").hidden = true;
  renderAll();
}

function stepFind(delta) {
  const n = findState.matches.length;
  if (!n) return;
  findState.cur = (findState.cur + delta + n) % n;
  updateFindBar();
  revealCurrentMatch();
}

function syncTabs() {
  document.querySelectorAll("#tabs .tab").forEach(t => {
    t.classList.toggle("active", t.dataset.view === activeView);
  });
}

function renderSidebar() {
  if (!world) return;
  const hasBuiltins = world.entities.some(e => e.name && BUILTINS.has(e.name));
  $("btnBuiltins").hidden = !hasBuiltins;
  $("btnBuiltins").classList.toggle("on", !hideBuiltins);

  const visible = world.entities.filter(e => !(hideBuiltins && e.name && BUILTINS.has(e.name)));
  const visSet = new Set(visible.map(e => e.slot));
  const hitSlots = new Set(findState.open ? findState.matches.map(m => m.slot) : []);
  const curMatch = findState.open && findState.cur >= 0 ? findState.matches[findState.cur] : null;

  $("treeCount").textContent = visible.length;
  $("prefabCount").textContent = world.prefabs.length;
  $("tabArchCnt").textContent = world.stats.archetypes;
  $("tabCompCnt").textContent = world.stats.components;
  $("footMeta").textContent = `${world.stats.entities} ent · ${world.stats.archetypes} tbl`;

  const roots = visible.filter(e => !e.parents.length || !e.parents.some(p => visSet.has(p)));

  function renderNode(e) {
    const kids = (e.children || []).map(entityBySlot).filter(c => c && visSet.has(c.slot));
    const hasKids = kids.length > 0;
    const isShut = collapsed.has(e.slot);
    const isSel = selectedSlot === e.slot;
    const isHit = hitSlots.has(e.slot);
    const isCur = curMatch && curMatch.slot === e.slot;
    const cls = ["row", isHit ? "find-hit" : "", isCur ? "find-cur" : ""].filter(Boolean).join(" ");
    let html = `<div class="${cls}" data-slot="${e.slot}" ${isSel ? 'aria-current="page"' : ""}>` +
      `<span class="twisty ${hasKids ? (isShut ? "shut" : "") : "leaf"}" data-toggle="${e.slot}">` +
      `<svg width="10" height="10" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5l3 3 3-3"/></svg></span>` +
      (e.is_prefab ? `<span class="pdot" title="Prefab"></span>` : "") +
      `<span class="label">${highlightText(labelOf(e), isCur)}</span>` +
      `<span class="num">#${e.slot}</span>` +
      `</div>`;
    if (hasKids && !isShut) {
      html += `<div class="kids">${kids.map(renderNode).join("")}</div>`;
    }
    return html;
  }

  $("tree").innerHTML = roots.length
    ? roots.map(renderNode).join("")
    : `<div class="empty" style="padding:12px 10px;font-size:12.5px">No entities</div>`;

  $("prefabs").innerHTML = world.prefabs.length
    ? world.prefabs.map(p =>
      `<div class="row" data-slot="${p.slot}" ${selectedSlot === p.slot ? 'aria-current="page"' : ""}>` +
        `<span class="pdot"></span>` +
        `<span class="label">${esc(labelOf(p))}</span>` +
        `<button class="inst" data-inst="${p.slot}" type="button">Instantiate</button>` +
      `</div>`
    ).join("")
    : `<div class="empty" style="padding:8px 10px;font-size:12.5px">None</div>`;

  $("tree").querySelectorAll(".row").forEach(el => {
    el.addEventListener("click", (ev) => {
      const tog = ev.target.closest("[data-toggle]");
      if (tog && !tog.classList.contains("leaf")) {
        ev.stopPropagation();
        const s = Number(tog.dataset.toggle);
        if (collapsed.has(s)) collapsed.delete(s); else collapsed.add(s);
        renderSidebar();
        return;
      }
      selectedSlot = Number(el.dataset.slot);
      activeView = "inspector";
      syncTabs();
      renderAll();
    });
  });

  $("prefabs").querySelectorAll(".row").forEach(el => {
    el.addEventListener("click", (ev) => {
      if (ev.target.closest("[data-inst]")) return;
      selectedSlot = Number(el.dataset.slot);
      activeView = "inspector";
      syncTabs();
      renderAll();
    });
  });
  $("prefabs").querySelectorAll("[data-inst]").forEach(btn => {
    btn.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      const pSlot = Number(btn.dataset.inst);
      const p = entityBySlot(pSlot);
      const name = `${p && p.name ? p.name : "Entity"}_${world.stats.entities + 1}`;
      const r = await api("POST", "/api/entity", { name, prefab: pSlot });
      if (r.ok && r.json) {
        selectedSlot = r.json.slot;
        activeView = "inspector";
        syncTabs();
        await refresh();
      }
    });
  });
}

function renderInspector() {
  const root = $("viewRoot");
  const e = entityBySlot(selectedSlot);
  if (!e) {
    root.innerHTML = `<div class="empty">Select an entity from the hierarchy to inspect and live-edit its components.</div>`;
    return;
  }

  // Build lineage breadcrumb
  const chain = [];
  let walk = e, guard = 0;
  while (walk && guard++ < 16) {
    chain.unshift(walk);
    walk = walk.parents && walk.parents.length ? entityBySlot(walk.parents[0]) : null;
  }
  const crumbHtml = chain.map((node, i) => {
    const isLast = i === chain.length - 1;
    const part = isLast
      ? `<span>${esc(labelOf(node))}</span>`
      : `<a data-jump="${node.slot}">${esc(labelOf(node))}</a>`;
    return (i > 0 ? `<span class="sep">/</span>` : "") + part;
  }).join("");

  const curParent = e.parents.length ? e.parents[0] : "";
  const parentOpts = [`<option value="">None (root)</option>`]
    .concat(world.entities
      .filter(x => x.slot !== e.slot && !(hideBuiltins && x.name && BUILTINS.has(x.name)))
      .map(x => `<option value="${x.slot}" ${x.slot === curParent ? "selected" : ""}>${esc(labelOf(x))} (#${x.slot})</option>`))
    .join("");

  const prefabChips = (e.prefabs || []).map(ps => {
    const p = entityBySlot(ps);
    return `<span class="badge fire">IsA <a data-jump="${ps}">${esc(p ? labelOf(p) : "#" + ps)}</a></span>`;
  }).join("");

  const tagsHtml = (e.tags || []).map(t =>
    `<span class="badge">#${highlightText(t, false)}<button data-rm-tag="${esc(t)}" type="button" title="Remove tag">×</button></span>`
  ).join("");

  const compEntries = Object.entries(e.components || {});
  const curMatch = findState.open && findState.cur >= 0 ? findState.matches[findState.cur] : null;
  const attachable = world.components.filter(c => c.reflected && !(c.name in (e.components || {})));

  const compsHtml = compEntries.length ? compEntries.map(([cname, cval]) => {
    const meta = compMeta(cname);
    const schema = schemaOf(cname);
    const sz = meta ? `${meta.size}B` : "";
    const isCompCur = curMatch && curMatch.slot === e.slot && curMatch.comp === cname;
    let body = "";
    if (cval === "<opaque>") {
      body = `<div class="frow"><span class="fname">value</span><span class="mono" style="color:var(--ink-4);font-size:12px">opaque (unreflected)</span><span class="ftype"></span></div>`;
    } else if (schema && schema.kind === "struct" && Array.isArray(schema.fields)) {
      body = schema.fields.map(f => {
        const fv = cval && typeof cval === "object" ? cval[f.name] : "";
        const isFieldCur = isCompCur && curMatch.field === f.name;
        const isBool = f.type === "bool" || typeof fv === "boolean";
        const control = isBool
          ? `<label class="switch"><input type="checkbox" data-comp="${esc(cname)}" data-field="${esc(f.name)}" data-type="bool" ${fv ? "checked" : ""}><span class="track"></span><span class="switch-val">${fv ? "true" : "false"}</span></label>`
          : `<input class="finput" type="${/^(f|i|u)/.test(f.type) ? "number" : "text"}" step="any" data-comp="${esc(cname)}" data-field="${esc(f.name)}" data-type="${esc(f.type)}" value="${esc(fv)}">`;
        return `<div class="frow"><span class="fname">${highlightText(f.name, isFieldCur)}</span>${control}<span class="ftype">${esc(f.type)}</span></div>`;
      }).join("");
    } else if (cval && typeof cval === "object" && !Array.isArray(cval)) {
      body = Object.entries(cval).map(([fk, fv]) => {
        const isBool = typeof fv === "boolean";
        const control = isBool
          ? `<label class="switch"><input type="checkbox" data-comp="${esc(cname)}" data-field="${esc(fk)}" data-type="bool" ${fv ? "checked" : ""}><span class="track"></span><span class="switch-val">${fv ? "true" : "false"}</span></label>`
          : `<input class="finput" type="${typeof fv === "number" ? "number" : "text"}" step="any" data-comp="${esc(cname)}" data-field="${esc(fk)}" data-type="${typeof fv === "number" ? "f64" : "str"}" value="${esc(fv)}">`;
        return `<div class="frow"><span class="fname">${highlightText(fk, false)}</span>${control}<span class="ftype">${typeof fv}</span></div>`;
      }).join("");
    } else {
      const ftype = schema && schema.type ? schema.type : typeof cval;
      body = `<div class="frow"><span class="fname">value</span><input class="finput" data-comp="${esc(cname)}" data-direct="1" data-type="${esc(ftype)}" value="${esc(cval)}"><span class="ftype">${esc(ftype)}</span></div>`;
    }
    return `<details class="comp-sec" open>
      <summary>
        <svg class="chev" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5l3 3 3-3"/></svg>
        <span class="comp-name">${highlightText(cname, isCompCur)}</span>
        <span class="comp-size">${sz}</span>
        <span class="comp-status" id="st_${esc(cname)}"></span>
      </summary>
      <div class="fields">${body}</div>
    </details>`;
  }).join("") : `<div class="empty" style="padding:20px 0">No components attached.</div>`;

  const attachHtml = attachable.length
    ? `<div style="display:flex;align-items:center;gap:6px">
         <select class="quiet-select" id="attachSel">${attachable.map(c => `<option value="${esc(c.name)}">${esc(c.name)}</option>`).join("")}</select>
         <button class="btn-subtle" id="btnAttach" type="button">+ Attach</button>
       </div>`
    : "";

  root.innerHTML = `
    <div class="head">
      <div class="crumb">${crumbHtml}</div>
      <div class="head-row">
        <div>
          <h1>${highlightText(labelOf(e), curMatch && curMatch.slot === e.slot && curMatch.kind === "entity")}</h1>
          <div class="head-sub">
            <span>slot ${e.slot} · gen ${e.gen} · table #${e.table_id}</span>
            ${e.is_prefab ? `<span class="badge fire">Prefab</span>` : ""}
            ${prefabChips}
          </div>
        </div>
        <button class="ghost danger" id="btnDeleteEnt" type="button">Delete</button>
      </div>
    </div>

    <div class="meta-strip">
      <div class="meta-group">
        <span class="meta-label">Parent</span>
        <select class="quiet-select" id="selParent">${parentOpts}</select>
      </div>
      <div class="meta-group">
        <span class="meta-label">Tags</span>
        ${tagsHtml}
        <input class="inline-add" id="inTag" placeholder="+ add tag">
      </div>
    </div>

    <div class="section-bar">
      <h2>Components (${compEntries.length})</h2>
      ${attachHtml}
    </div>
    ${compsHtml}
  `;

  root.querySelectorAll("[data-jump]").forEach(a => {
    a.addEventListener("click", () => {
      selectedSlot = Number(a.dataset.jump);
      renderAll();
    });
  });

  $("selParent").addEventListener("change", async () => {
    const p = $("selParent").value;
    const r = p === ""
      ? await api("DELETE", `/api/entity/${e.slot}/parent`)
      : await api("POST", `/api/entity/${e.slot}/parent/${p}`);
    if (!r.ok) alert((r.json && r.json.error) || `HTTP ${r.status}`);
    await refresh();
  });

  $("inTag").addEventListener("keydown", async (ev) => {
    if (ev.key !== "Enter") return;
    const t = $("inTag").value.trim();
    if (!t) return;
    await api("POST", `/api/entity/${e.slot}/tag/${encodeURIComponent(t)}`);
    await refresh();
  });

  root.querySelectorAll("[data-rm-tag]").forEach(btn => {
    btn.addEventListener("click", async () => {
      await api("DELETE", `/api/entity/${e.slot}/tag/${encodeURIComponent(btn.dataset.rmTag)}`);
      await refresh();
    });
  });

  $("btnDeleteEnt").addEventListener("click", async () => {
    await api("DELETE", `/api/entity/${e.slot}`);
    selectedSlot = null;
    await refresh();
  });

  if ($("btnAttach")) {
    $("btnAttach").addEventListener("click", async () => {
      const cname = $("attachSel").value;
      const sch = schemaOf(cname);
      let def = 0;
      if (sch && sch.kind === "struct") {
        def = {};
        for (const f of sch.fields || []) {
          def[f.name] = f.type === "bool" ? false : 0;
        }
      }
      const r = await api("PUT", `/api/entity/${e.slot}/component/${encodeURIComponent(cname)}`, def);
      if (!r.ok) alert((r.json && r.json.error) || `HTTP ${r.status}`);
      await refresh();
    });
  }

  root.querySelectorAll("[data-comp]").forEach(inp => {
    const commit = async () => {
      const cname = inp.dataset.comp;
      const ftype = inp.dataset.type || "";
      const st = $(`st_${cname}`);
      let val;
      if (inp.type === "checkbox") {
        val = inp.checked;
        const lbl = inp.parentElement && inp.parentElement.querySelector(".switch-val");
        if (lbl) lbl.textContent = val ? "true" : "false";
      } else if (/^(f32|f64)$/.test(ftype)) {
        val = Number(inp.value);
      } else if (/^(i|u)(8|16|32|64|size)$/.test(ftype)) {
        val = Math.trunc(Number(inp.value));
      } else {
        val = inp.value;
      }
      const payload = inp.dataset.direct ? val : { [inp.dataset.field]: val };
      const r = await api("PATCH", `/api/entity/${e.slot}/component/${encodeURIComponent(cname)}`, payload);
      if (r.ok) {
        if (st) { st.className = "comp-status ok"; st.textContent = "saved"; }
        if (r.json) {
          const idx = world.entities.findIndex(x => x.slot === e.slot);
          if (idx >= 0) world.entities[idx] = r.json;
        }
        setTimeout(() => { if (st && st.textContent === "saved") st.textContent = ""; }, 1200);
      } else {
        if (st) { st.className = "comp-status err"; st.textContent = (r.json && r.json.error) || `err ${r.status}`; }
      }
    };
    inp.addEventListener("change", commit);
    inp.addEventListener("keydown", (ev) => { if (ev.key === "Enter") { ev.preventDefault(); inp.blur(); } });
  });
}

function renderArchetypes() {
  const root = $("viewRoot");
  const rows = world.archetypes.map(a => {
    const sig = a.components.length
      ? a.components.map(c => `<span class="sig-chip${c.startsWith("(") || c.startsWith("#") ? " rel" : ""}">${esc(c)}</span>`).join("")
      : `<span class="mono" style="color:var(--ink-4)">[]</span>`;
    return `<tr>
      <td class="mono" style="color:var(--ink-3)">#${a.id}</td>
      <td class="mono" style="color:var(--ink-1);font-weight:500">${a.entity_count}</td>
      <td class="mono" style="color:var(--ink-4)">+${a.add_edges}/-${a.remove_edges}</td>
      <td><div class="sig-list">${sig}</div></td>
    </tr>`;
  }).join("");
  root.innerHTML = `
    <div class="head">
      <h1>Archetypes</h1>
      <div class="head-sub"><span>${world.archetypes.length} tables · ${world.stats.entities} entities</span></div>
    </div>
    <table class="etable">
      <thead><tr><th style="width:64px">Table</th><th style="width:72px">Entities</th><th style="width:76px">Edges</th><th>Signature</th></tr></thead>
      <tbody>${rows}</tbody>
    </table>
  `;
}

function renderQuery() {
  const root = $("viewRoot");
  const res = queryState.result;
  let resultsHtml = `<div class="empty" style="padding:18px 0">Filter entities by component or tag names (comma-separated).</div>`;
  if (res) {
    resultsHtml = `<div class="head-sub" style="margin:0 0 8px"><span>${res.count} matching ${res.count === 1 ? "entity" : "entities"}</span></div>` +
      `<table class="etable"><thead><tr><th>Entity</th><th>Tags</th><th>Components</th></tr></thead><tbody>` +
      res.entities.map(e => `<tr style="cursor:pointer" data-qslot="${e.slot}">
        <td style="color:var(--ink-1);font-weight:500">${esc(labelOf(e))} <span class="mono" style="color:var(--ink-4);font-weight:400">#${e.slot}</span></td>
        <td class="mono" style="color:var(--ink-3);font-size:12px">${esc((e.tags || []).map(t => "#" + t).join(" "))}</td>
        <td class="mono" style="color:var(--ink-3);font-size:12px">${esc(Object.keys(e.components || {}).join(", "))}</td>
      </tr>`).join("") + `</tbody></table>`;
  }
  root.innerHTML = `
    <div class="head" style="padding-bottom:8px"><h1>Query</h1></div>
    <div class="qbar">
      <input class="qinput" id="qWith" placeholder="With: Transform, Health" value="${esc(queryState.with)}">
      <input class="qinput" id="qWithout" placeholder="Without: Docked" value="${esc(queryState.without)}">
      <button class="btn-primary" id="btnRunQ" type="button">Run</button>
    </div>
    ${resultsHtml}
  `;
  const run = async () => {
    queryState.with = $("qWith").value.trim();
    queryState.without = $("qWithout").value.trim();
    const qs = [];
    if (queryState.with) qs.push("with=" + encodeURIComponent(queryState.with));
    if (queryState.without) qs.push("without=" + encodeURIComponent(queryState.without));
    const r = await api("GET", "/api/query" + (qs.length ? "?" + qs.join("&") : ""));
    if (r.ok && r.json) { queryState.result = r.json; renderQuery(); }
  };
  $("btnRunQ").addEventListener("click", run);
  $("qWith").addEventListener("keydown", ev => { if (ev.key === "Enter") run(); });
  $("qWithout").addEventListener("keydown", ev => { if (ev.key === "Enter") run(); });
  root.querySelectorAll("[data-qslot]").forEach(tr => {
    tr.addEventListener("click", () => {
      selectedSlot = Number(tr.dataset.qslot);
      activeView = "inspector";
      syncTabs();
      renderAll();
    });
  });
}

function renderSchemas() {
  const root = $("viewRoot");
  const blocks = world.components.map(c => {
    const s = c.schema;
    if (s && s.kind === "struct" && Array.isArray(s.fields)) {
      const fields = s.fields.map(f =>
        `    <span class="kw">pub</span> ${esc(f.name)}: <span class="ty">${esc(f.type)}</span>,`
      ).join("\n");
      return `<pre class="schema-block"><span class="cm">// id #${c.id} · ${c.size}B · auto-Copy · reflected via inline for</span>\n<span class="kw">struct</span> <span class="ty">${esc(c.name)}</span> {\n${fields}\n}</pre>`;
    }
    const kind = s && s.type ? s.type : "opaque";
    return `<pre class="schema-block"><span class="cm">// id #${c.id} · ${c.size}B</span>\n<span class="kw">struct</span> <span class="ty">${esc(c.name)}</span>(<span class="ty">${esc(kind)}</span>);</pre>`;
  }).join("");
  root.innerHTML = `
    <div class="head">
      <h1>Schemas</h1>
      <div class="head-sub"><span>${world.components.length} registered components · checked by for&lt;F in fields(T)&gt;</span></div>
    </div>
    ${blocks || `<div class="empty">No components registered.</div>`}
  `;
}

function renderView() {
  if (!world) return;
  if (activeView === "inspector") renderInspector();
  else if (activeView === "archetypes") renderArchetypes();
  else if (activeView === "query") renderQuery();
  else renderSchemas();
}

function renderAll() {
  renderSidebar();
  renderView();
}

async function refresh() {
  const r = await api("GET", "/api/world");
  if (!r.ok || !r.json) return;
  world = r.json;
  if (selectedSlot === null || !entityBySlot(selectedSlot)) {
    const pick = world.entities.find(e => e.name && !BUILTINS.has(e.name) && !e.is_prefab && Object.keys(e.components || {}).length > 1)
      || world.entities.find(e => !(hideBuiltins && e.name && BUILTINS.has(e.name)))
      || world.entities[0];
    selectedSlot = pick ? pick.slot : null;
  }
  if (findState.open) recomputeFind(true);
  renderAll();
}

// Smooth hover glide pill on sidebar rows (matching shell.css .glide-hov)
(() => {
  const list = $("navList");
  const hov = $("glideHov");
  list.addEventListener("pointermove", (ev) => {
    const row = ev.target.closest(".row");
    if (!row || row.getAttribute("aria-current") === "page") {
      hov.style.opacity = "0";
      return;
    }
    const lr = list.getBoundingClientRect();
    const rr = row.getBoundingClientRect();
    hov.style.transform = `translate3d(${rr.left - lr.left}px, ${rr.top - lr.top}px, 0)`;
    hov.style.width = `${rr.width}px`;
    hov.style.height = `${rr.height}px`;
    hov.style.opacity = "1";
  });
  list.addEventListener("pointerleave", () => { hov.style.opacity = "0"; });
})();

// Tabs
$("tabs").addEventListener("click", (ev) => {
  const t = ev.target.closest(".tab");
  if (!t) return;
  activeView = t.dataset.view;
  syncTabs();
  renderView();
});

// Top & footer controls
$("btnRefresh").addEventListener("click", refresh);
$("btnBuiltins").addEventListener("click", () => { hideBuiltins = !hideBuiltins; renderSidebar(); });
$("btnAuto").addEventListener("click", () => {
  if (autoTimer) {
    clearInterval(autoTimer);
    autoTimer = null;
    $("btnAuto").classList.remove("on");
  } else {
    autoTimer = setInterval(refresh, 1500);
    $("btnAuto").classList.add("on");
  }
});
$("btnTheme").addEventListener("click", () => {
  const cur = document.documentElement.getAttribute("data-theme");
  const prefersLight = window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches;
  const isLight = cur ? cur === "light" : prefersLight;
  document.documentElement.setAttribute("data-theme", isLight ? "dark" : "light");
});

// Find bar (Cmd+F / Ctrl+F / button)
$("btnOpenFind").addEventListener("click", openFind);
$("findClose").addEventListener("click", closeFind);
$("findPrev").addEventListener("click", () => stepFind(-1));
$("findNext").addEventListener("click", () => stepFind(1));
$("findInput").addEventListener("input", () => {
  findState.query = $("findInput").value;
  recomputeFind(false);
  if (findState.cur >= 0) revealCurrentMatch();
  else renderAll();
});
$("findInput").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") { ev.preventDefault(); stepFind(ev.shiftKey ? -1 : 1); }
  else if (ev.key === "Escape") { ev.preventDefault(); closeFind(); }
});
window.addEventListener("keydown", (ev) => {
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "f") {
    ev.preventDefault();
    openFind();
  } else if (ev.key === "Escape" && findState.open) {
    closeFind();
  }
});

// Export modal
async function openExport(fmt) {
  const r = await api("GET", fmt === "tab" ? "/api/world.tab" : "/api/world");
  $("exportTitle").textContent = fmt === "tab" ? "World Snapshot (TAB)" : "World Snapshot (JSON)";
  $("exportPre").textContent = fmt === "json" && r.json ? JSON.stringify(r.json, null, 2) : r.text;
  $("exportModal").classList.add("open");
}
$("btnJson").addEventListener("click", () => openExport("json"));
$("btnTab").addEventListener("click", () => openExport("tab"));
$("btnCloseExport").addEventListener("click", () => $("exportModal").classList.remove("open"));
$("btnDoneExport").addEventListener("click", () => $("exportModal").classList.remove("open"));
$("btnCopyExport").addEventListener("click", () => {
  navigator.clipboard && navigator.clipboard.writeText($("exportPre").textContent);
});

// Spawn modal
$("btnSpawn").addEventListener("click", () => {
  if (!world) return;
  $("spErr").textContent = "";
  $("spName").value = "";
  $("spTags").value = "";
  $("spParent").innerHTML = `<option value="">None (root)</option>` +
    world.entities.filter(e => !(hideBuiltins && e.name && BUILTINS.has(e.name)))
      .map(e => `<option value="${e.slot}" ${e.slot === selectedSlot ? "selected" : ""}>${esc(labelOf(e))} (#${e.slot})</option>`).join("");
  $("spPrefab").innerHTML = `<option value="">None</option>` +
    world.prefabs.map(p => `<option value="${p.slot}">${esc(labelOf(p))} (#${p.slot})</option>`).join("");
  $("spawnModal").classList.add("open");
  $("spName").focus();
});
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
  if (r.json) { selectedSlot = r.json.slot; activeView = "inspector"; syncTabs(); }
  await refresh();
});

refresh();
</script>
</body>
</html>
"##;
