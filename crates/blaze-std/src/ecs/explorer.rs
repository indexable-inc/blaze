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
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Blaze ECS Explorer</title>
<style>
:root {
  --bg: #0b0f17;
  --panel: #111827;
  --panel-alt: #161f32;
  --border: #243046;
  --text: #e5ecf6;
  --muted: #8b9bb4;
  --accent: #38bdf8;
  --accent-dim: rgba(56, 189, 248, 0.14);
  --green: #34d399;
  --amber: #fbbf24;
  --red: #f87171;
  --purple: #c084fc;
  --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  --sans: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
}
* { box-sizing: border-box; }
html, body {
  margin: 0; padding: 0; height: 100%; background: var(--bg); color: var(--text);
  font-family: var(--sans); font-size: 13px; overflow: hidden;
}
button, input, select, textarea {
  font-family: inherit; font-size: 12px; color: var(--text);
  background: var(--bg); border: 1px solid var(--border); border-radius: 5px;
  padding: 5px 8px; outline: none;
}
input:focus, select:focus, textarea:focus { border-color: var(--accent); }
button {
  cursor: pointer; background: var(--panel-alt); font-weight: 500;
  transition: background 0.12s, border-color 0.12s;
}
button:hover { border-color: var(--accent); background: #1c2840; }
button.primary { background: #0ea5e9; border-color: #38bdf8; color: #04131f; font-weight: 600; }
button.primary:hover { background: #38bdf8; }
button.danger { border-color: rgba(248, 113, 113, 0.5); color: var(--red); }
button.danger:hover { background: rgba(248, 113, 113, 0.14); border-color: var(--red); }
button.active { background: var(--accent-dim); border-color: var(--accent); color: var(--accent); }

.app { display: grid; grid-template-rows: 48px 1fr; height: 100vh; }
header {
  display: flex; align-items: center; justify-content: space-between; gap: 12px;
  padding: 0 14px; background: var(--panel); border-bottom: 1px solid var(--border);
}
.brand { display: flex; align-items: center; gap: 8px; font-weight: 700; font-size: 14px; letter-spacing: 0.02em; }
.brand-dot { width: 9px; height: 9px; border-radius: 50%; background: var(--accent); box-shadow: 0 0 10px var(--accent); }
.stats { display: flex; gap: 8px; flex-wrap: wrap; }
.stat-pill {
  background: var(--bg); border: 1px solid var(--border); border-radius: 999px;
  padding: 3px 10px; font-family: var(--mono); font-size: 11px; color: var(--muted);
}
.stat-pill b { color: var(--text); margin-left: 4px; }
.toolbar { display: flex; align-items: center; gap: 6px; }

.workspace {
  display: grid; grid-template-columns: 290px 1fr 400px; height: calc(100vh - 48px);
}
.pane {
  display: flex; flex-direction: column; min-height: 0; border-right: 1px solid var(--border);
  background: var(--panel);
}
.pane:last-child { border-right: none; }
.pane-header {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding: 8px 10px; border-bottom: 1px solid var(--border); background: var(--panel-alt);
  font-weight: 600; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted);
}
.pane-body { flex: 1; overflow-y: auto; padding: 10px; min-height: 0; }

.search-row { display: flex; gap: 6px; padding: 8px 10px; border-bottom: 1px solid var(--border); }
.search-row input { flex: 1; }

.tree-node {
  display: flex; align-items: center; gap: 5px; padding: 4px 6px; border-radius: 5px;
  cursor: pointer; user-select: none; margin-bottom: 1px;
}
.tree-node:hover { background: rgba(255,255,255,0.04); }
.tree-node.selected { background: var(--accent-dim); outline: 1px solid rgba(56,189,248,0.45); }
.caret { width: 14px; text-align: center; color: var(--muted); font-size: 10px; flex-shrink: 0; }
.ent-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.slot-tag { font-family: var(--mono); font-size: 11px; color: var(--muted); }
.badge {
  font-family: var(--mono); font-size: 10px; padding: 1px 5px; border-radius: 4px;
  border: 1px solid var(--border); background: var(--bg); color: var(--muted);
}
.badge.prefab { border-color: rgba(192,132,252,0.45); color: var(--purple); background: rgba(192,132,252,0.1); }
.badge.comp { border-color: rgba(56,189,248,0.35); color: var(--accent); }
.badge.tag { border-color: rgba(251,191,36,0.35); color: var(--amber); }
.badge.ok { border-color: rgba(52,211,153,0.45); color: var(--green); }
.badge.err { border-color: rgba(248,113,113,0.45); color: var(--red); }

.section-title {
  font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted);
  margin: 12px 0 6px 0; font-weight: 600; display: flex; justify-content: space-between; align-items: center;
}
.section-title:first-child { margin-top: 0; }
.prefab-item {
  display: flex; align-items: center; justify-content: space-between; gap: 6px;
  padding: 5px 7px; border: 1px solid var(--border); border-radius: 6px; margin-bottom: 5px;
  background: var(--bg);
}

.card {
  background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
  padding: 10px 12px; margin-bottom: 10px;
}
.card-head {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  margin-bottom: 8px; font-weight: 600;
}
.chips { display: flex; flex-wrap: wrap; gap: 5px; align-items: center; }
.chip {
  display: inline-flex; align-items: center; gap: 5px; padding: 2px 7px; border-radius: 999px;
  border: 1px solid var(--border); background: var(--panel-alt); font-size: 11px;
}
.chip button { border: none; background: transparent; padding: 0 2px; color: var(--muted); font-size: 13px; }
.chip button:hover { color: var(--red); background: transparent; }

.field-grid { display: grid; grid-template-columns: 120px 1fr 70px; gap: 6px 8px; align-items: center; }
.field-label { font-family: var(--mono); font-size: 12px; color: var(--text); }
.field-type { font-family: var(--mono); font-size: 11px; color: var(--muted); text-align: right; }
.row { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }

.tabs { display: flex; gap: 4px; }
.tab-btn { padding: 4px 8px; font-size: 11px; text-transform: none; letter-spacing: 0; }
table.grid { width: 100%; border-collapse: collapse; font-size: 12px; }
table.grid th, table.grid td {
  text-align: left; padding: 6px 7px; border-bottom: 1px solid var(--border); vertical-align: top;
}
table.grid th { color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; }
table.grid tr.clickable { cursor: pointer; }
table.grid tr.clickable:hover { background: rgba(56,189,248,0.08); }

.empty-state { color: var(--muted); text-align: center; padding: 36px 16px; }
.modal-backdrop {
  position: fixed; inset: 0; background: rgba(2, 6, 14, 0.72); display: none;
  align-items: center; justify-content: center; z-index: 40;
}
.modal-backdrop.open { display: flex; }
.modal {
  width: min(680px, 92vw); max-height: 84vh; display: flex; flex-direction: column;
  background: var(--panel); border: 1px solid var(--border); border-radius: 10px; overflow: hidden;
}
.modal-body { padding: 12px; overflow-y: auto; display: flex; flex-direction: column; gap: 8px; }
pre.code {
  margin: 0; padding: 10px; background: var(--bg); border: 1px solid var(--border);
  border-radius: 6px; font-family: var(--mono); font-size: 11px; overflow: auto; max-height: 55vh;
  white-space: pre-wrap; word-break: break-word;
}
</style>
</head>
<body>
<div class="app">
  <header>
    <div class="brand"><span class="brand-dot"></span> Blaze ECS Explorer</div>
    <div class="stats" id="statsBar"></div>
    <div class="toolbar">
      <button id="btnHideBuiltins" class="active" title="Toggle built-in relation/trait entities">Hide Builtins</button>
      <button id="btnAuto">Auto-Refresh: Off</button>
      <button id="btnRefresh">Refresh</button>
      <button id="btnExportJson">JSON</button>
      <button id="btnExportTab">TAB</button>
    </div>
  </header>

  <div class="workspace">
    <!-- Left Pane: Entity Hierarchy Tree & Prefabs -->
    <section class="pane">
      <div class="pane-header">
        <span>Entities &amp; Hierarchy</span>
        <button id="btnOpenSpawn" class="primary">+ Spawn</button>
      </div>
      <div class="search-row">
        <input id="searchInput" type="text" placeholder="Filter name, #slot, tag, component..." />
      </div>
      <div class="pane-body">
        <div class="section-title"><span>ChildOf Tree</span><span id="treeCount" class="slot-tag"></span></div>
        <div id="entityTree"></div>
        <div class="section-title"><span>IsA Prefabs</span><span id="prefabCount" class="slot-tag"></span></div>
        <div id="prefabList"></div>
      </div>
    </section>

    <!-- Center Pane: Live Entity Inspector -->
    <section class="pane">
      <div class="pane-header">
        <span id="inspectorTitle">Entity Inspector</span>
        <span id="inspectorStatus" class="badge"></span>
      </div>
      <div class="pane-body" id="inspectorBody">
        <div class="empty-state">Select an entity from the hierarchy tree or query results to inspect and live-edit components.</div>
      </div>
    </section>

    <!-- Right Pane: Archetypes, Live Query Runner & Schemas -->
    <section class="pane">
      <div class="pane-header">
        <div class="tabs">
          <button class="tab-btn active" data-tab="archetypes">Archetypes</button>
          <button class="tab-btn" data-tab="query">Query Runner</button>
          <button class="tab-btn" data-tab="schemas">Schemas</button>
        </div>
      </div>
      <div class="pane-body" id="rightPaneBody"></div>
    </section>
  </div>
</div>

<!-- Export Modal -->
<div class="modal-backdrop" id="exportModal">
  <div class="modal">
    <div class="pane-header">
      <span id="exportTitle">Export</span>
      <div class="row">
        <button id="btnCopyExport">Copy</button>
        <button id="btnDownloadExport">Download</button>
        <button id="btnCloseExport">Close</button>
      </div>
    </div>
    <div class="modal-body">
      <pre class="code" id="exportPre"></pre>
    </div>
  </div>
</div>

<!-- Spawn Entity Modal -->
<div class="modal-backdrop" id="spawnModal">
  <div class="modal">
    <div class="pane-header">
      <span>Spawn Entity</span>
      <button id="btnCloseSpawn">Close</button>
    </div>
    <div class="modal-body">
      <label>Name (optional) <input id="spName" style="width:100%;margin-top:3px" placeholder="e.g. Player1" /></label>
      <div class="row">
        <label style="flex:1">Parent Slot (optional) <input id="spParent" type="number" style="width:100%;margin-top:3px" /></label>
        <label style="flex:1">Prefab Slot (optional) <input id="spPrefab" type="number" style="width:100%;margin-top:3px" /></label>
      </div>
      <label>Tags (comma-separated) <input id="spTags" style="width:100%;margin-top:3px" placeholder="Alive, Active" /></label>
      <label>Components JSON (optional)
        <textarea id="spComps" rows="4" style="width:100%;margin-top:3px;font-family:var(--mono)" placeholder='{"Pos": {"x": 1.0, "y": 2.0}}'></textarea>
      </label>
      <div id="spErr" style="color:var(--red);font-size:12px"></div>
      <div class="row" style="justify-content:flex-end">
        <button id="btnDoSpawn" class="primary">Create Entity</button>
      </div>
    </div>
  </div>
</div>

<script>
const BUILTIN_NAMES = new Set(["ChildOf","IsA","Prefab","Wildcard","Exclusive","Transitive","Symmetric","OnDeleteCascade"]);
let worldData = null;
let selectedSlot = null;
let collapsedSlots = new Set();
let hideBuiltins = true;
let activeTab = "archetypes";
let autoTimer = null;
let lastQuery = { with: "", without: "", includePrefabs: false, result: null };
let lastExport = { name: "world.json", text: "" };

const $ = (id) => document.getElementById(id);

function esc(s) {
  return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;");
}

function entityLabel(e) {
  return e.name ? e.name : `#${e.slot}`;
}

function entityMap() {
  const m = new Map();
  if (worldData) for (const e of worldData.entities) m.set(e.slot, e);
  return m;
}

function compMetaMap() {
  const m = new Map();
  if (worldData) for (const c of worldData.components) m.set(c.name, c);
  return m;
}

async function api(method, path, bodyObj) {
  const opts = { method, headers: {} };
  if (bodyObj !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = typeof bodyObj === "string" ? bodyObj : JSON.stringify(bodyObj);
  }
  const res = await fetch(path, opts);
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch (_) {}
  return { ok: res.ok, status: res.status, text, json };
}

async function refresh() {
  const r = await api("GET", "/api/world");
  if (!r.ok || !r.json) return;
  worldData = r.json;
  renderStats();
  renderTreeAndPrefabs();
  renderInspector();
  renderRightPane();
}

function renderStats() {
  const s = worldData.stats;
  $("statsBar").innerHTML = `
    <span class="stat-pill">Entities<b>${s.entities}</b></span>
    <span class="stat-pill">Archetypes<b>${s.archetypes}</b></span>
    <span class="stat-pill">Components<b>${s.components}</b></span>
    <span class="stat-pill">Prefabs<b>${s.prefabs}</b></span>
  `;
}

function isHiddenBuiltin(e) {
  return hideBuiltins && e.name && BUILTIN_NAMES.has(e.name);
}

function renderTreeAndPrefabs() {
  const q = $("searchInput").value.trim().toLowerCase();
  const bySlot = entityMap();
  const visible = worldData.entities.filter((e) => {
    if (isHiddenBuiltin(e)) return false;
    if (!q) return true;
    const hay = [
      e.name || "", `#${e.slot}`, String(e.slot),
      ...(e.tags || []), ...Object.keys(e.components || {})
    ].join(" ").toLowerCase();
    return hay.includes(q);
  });
  const visSet = new Set(visible.map((e) => e.slot));
  $("treeCount").textContent = `${visible.length} shown`;

  // Roots are visible entities whose parent is absent or not in visSet
  const roots = visible.filter((e) => {
    const p = e.parents && e.parents.length ? e.parents[0] : null;
    return p === null || !visSet.has(p);
  });

  const rows = [];
  const stack = [];
  for (let i = roots.length - 1; i >= 0; i--) stack.push({ ent: roots[i], depth: 0 });
  const visited = new Set();
  while (stack.length) {
    const { ent, depth } = stack.pop();
    if (visited.has(ent.slot)) continue;
    visited.add(ent.slot);
    const kids = (ent.children || []).map((c) => bySlot.get(c)).filter((c) => c && visSet.has(c.slot));
    const collapsed = collapsedSlots.has(ent.slot);
    const compCount = Object.keys(ent.components || {}).length;
    rows.push(`
      <div class="tree-node ${selectedSlot === ent.slot ? "selected" : ""}" style="padding-left:${6 + depth * 14}px" data-slot="${ent.slot}">
        <span class="caret" data-toggle="${ent.slot}">${kids.length ? (collapsed ? "&#9656;" : "&#9662;") : ""}</span>
        <span class="ent-name">${esc(entityLabel(ent))}</span>
        <span class="slot-tag">#${ent.slot}</span>
        ${ent.is_prefab ? `<span class="badge prefab">P</span>` : ""}
        ${compCount ? `<span class="badge comp">${compCount}c</span>` : ""}
      </div>
    `);
    if (!collapsed) {
      for (let i = kids.length - 1; i >= 0; i--) stack.push({ ent: kids[i], depth: depth + 1 });
    }
  }
  $("entityTree").innerHTML = rows.join("") || `<div class="empty-state">No entities match.</div>`;
  $("entityTree").querySelectorAll(".tree-node").forEach((el) => {
    el.addEventListener("click", (ev) => {
      const toggle = ev.target.getAttribute("data-toggle");
      if (toggle !== null && ev.target.textContent) {
        const s = Number(toggle);
        if (collapsedSlots.has(s)) collapsedSlots.delete(s); else collapsedSlots.add(s);
        renderTreeAndPrefabs();
        return;
      }
      selectedSlot = Number(el.getAttribute("data-slot"));
      renderTreeAndPrefabs();
      renderInspector();
    });
  });

  const prefabs = worldData.prefabs || [];
  $("prefabCount").textContent = String(prefabs.length);
  $("prefabList").innerHTML = prefabs.map((p) => `
    <div class="prefab-item">
      <div style="cursor:pointer" data-select-prefab="${p.slot}">
        <strong>${esc(entityLabel(p))}</strong> <span class="slot-tag">#${p.slot}</span>
      </div>
      <button class="primary" data-inst="${p.slot}">Instantiate</button>
    </div>
  `).join("") || `<div class="slot-tag">No prefabs registered.</div>`;

  $("prefabList").querySelectorAll("[data-select-prefab]").forEach((el) => {
    el.addEventListener("click", () => {
      selectedSlot = Number(el.getAttribute("data-select-prefab"));
      renderTreeAndPrefabs();
      renderInspector();
    });
  });
  $("prefabList").querySelectorAll("[data-inst]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const slot = Number(btn.getAttribute("data-inst"));
      const r = await api("POST", "/api/entity", { prefab: slot });
      if (r.ok && r.json) {
        selectedSlot = r.json.slot;
        await refresh();
      }
    });
  });
}

function defaultForSchema(schema) {
  if (!schema) return null;
  if (schema.kind === "primitive") {
    return schema.type === "bool" ? false : 0;
  }
  if (schema.kind === "newtype") {
    return schema.type === "bool" ? false : 0;
  }
  if (schema.kind === "struct") {
    const o = {};
    for (const f of schema.fields || []) {
      o[f.name] = f.type === "bool" ? false : 0;
    }
    return o;
  }
  return null;
}

function renderInput(fieldName, fieldType, val, compName) {
  const safeVal = val === undefined || val === null ? "" : val;
  if (fieldType === "bool") {
    return `<input type="checkbox" data-comp="${esc(compName)}" data-field="${esc(fieldName)}" data-ftype="bool" ${val ? "checked" : ""} />`;
  }
  if (fieldType === "f32" || fieldType === "f64") {
    return `<input type="number" step="any" data-comp="${esc(compName)}" data-field="${esc(fieldName)}" data-ftype="float" value="${esc(safeVal)}" />`;
  }
  if (/^[iu](8|16|32|64|size)$/.test(fieldType)) {
    return `<input type="number" step="1" data-comp="${esc(compName)}" data-field="${esc(fieldName)}" data-ftype="int" value="${esc(safeVal)}" />`;
  }
  return `<input type="text" data-comp="${esc(compName)}" data-field="${esc(fieldName)}" data-ftype="json" value="${esc(typeof val === "object" ? JSON.stringify(val) : safeVal)}" />`;
}

function renderInspector() {
  const bySlot = entityMap();
  const ent = selectedSlot !== null ? bySlot.get(selectedSlot) : null;
  if (!ent) {
    $("inspectorTitle").textContent = "Entity Inspector";
    $("inspectorStatus").textContent = "";
    $("inspectorStatus").className = "badge";
    $("inspectorBody").innerHTML = `<div class="empty-state">Select an entity to inspect and live-edit its components, tags, and hierarchy.</div>`;
    return;
  }

  $("inspectorTitle").textContent = `${entityLabel(ent)} (#${ent.slot} v${ent.gen})`;
  const metas = compMetaMap();
  const curParent = ent.parents && ent.parents.length ? ent.parents[0] : null;
  const parentOptions = worldData.entities
    .filter((o) => o.slot !== ent.slot && !isHiddenBuiltin(o))
    .map((o) => `<option value="${o.slot}" ${curParent === o.slot ? "selected" : ""}>${esc(entityLabel(o))} (#${o.slot})</option>`)
    .join("");

  const existingComps = Object.keys(ent.components || {});
  const attachable = worldData.components.filter((c) => c.reflected && !existingComps.includes(c.name));

  let compsHtml = "";
  for (const cname of existingComps) {
    const val = ent.components[cname];
    const meta = metas.get(cname);
    const schema = meta ? meta.schema : null;
    let bodyHtml = "";
    if (val === "<opaque>" || !meta || !meta.reflected) {
      bodyHtml = `<span class="badge">&lt;opaque&gt; (not reflected)</span>`;
    } else if (schema && schema.kind === "struct") {
      bodyHtml = `<div class="field-grid">` + (schema.fields || []).map((f) => `
        <span class="field-label">${esc(f.name)}</span>
        ${renderInput(f.name, f.type, val ? val[f.name] : null, cname)}
        <span class="field-type">${esc(f.type)}</span>
      `).join("") + `</div>`;
    } else if (schema && (schema.kind === "primitive" || schema.kind === "newtype")) {
      bodyHtml = `<div class="field-grid">
        <span class="field-label">value</span>
        ${renderInput("", schema.type, val, cname)}
        <span class="field-type">${esc(schema.type)}</span>
      </div>`;
    } else {
      bodyHtml = `<pre class="code">${esc(JSON.stringify(val, null, 2))}</pre>`;
    }
    compsHtml += `
      <div class="card">
        <div class="card-head">
          <span>${esc(cname)} ${meta ? `<span class="slot-tag">${meta.size}B</span>` : ""}</span>
          <span class="badge" id="status-${esc(cname)}"></span>
        </div>
        ${bodyHtml}
      </div>
    `;
  }

  $("inspectorBody").innerHTML = `
    <div class="card">
      <div class="card-head">
        <div class="row">
          <span style="font-size:15px">${esc(entityLabel(ent))}</span>
          <span class="slot-tag">slot ${ent.slot} &middot; gen ${ent.gen} &middot; table #${ent.table_id}</span>
          ${ent.is_prefab ? `<span class="badge prefab">PREFAB</span>` : ""}
        </div>
        <button id="btnDeleteEnt" class="danger">Delete Entity</button>
      </div>
      <div class="row" style="margin-top:6px">
        <span class="slot-tag">Parent:</span>
        <select id="selParent"><option value="">(none)</option>${parentOptions}</select>
        <button id="btnSetParent">Set Parent</button>
        <button id="btnClearParent">Clear</button>
      </div>
      ${ent.children && ent.children.length ? `
        <div class="row" style="margin-top:8px">
          <span class="slot-tag">Children:</span>
          <div class="chips">${ent.children.map((c) => {
            const ce = bySlot.get(c);
            return `<span class="chip" style="cursor:pointer" data-jump="${c}">${esc(ce ? entityLabel(ce) : "#"+c)}</span>`;
          }).join("")}</div>
        </div>` : ""}
      ${ent.prefabs && ent.prefabs.length ? `
        <div class="row" style="margin-top:8px">
          <span class="slot-tag">IsA:</span>
          <div class="chips">${ent.prefabs.map((p) => {
            const pe = bySlot.get(p);
            return `<span class="chip" style="cursor:pointer" data-jump="${p}">${esc(pe ? entityLabel(pe) : "#"+p)}</span>`;
          }).join("")}</div>
        </div>` : ""}
    </div>

    <div class="card">
      <div class="card-head"><span>Tags</span></div>
      <div class="chips" style="margin-bottom:8px">
        ${(ent.tags || []).map((t) => `<span class="chip"><span class="badge tag">#${esc(t)}</span><button data-rm-tag="${esc(t)}" title="Remove tag">&times;</button></span>`).join("") || `<span class="slot-tag">None</span>`}
      </div>
      <div class="row">
        <input id="newTagInput" placeholder="Tag name..." />
        <button id="btnAddTag">+ Tag</button>
      </div>
    </div>

    <div class="section-title">
      <span>Components (${existingComps.length})</span>
      ${attachable.length ? `<div class="row">
        <select id="selAttachComp">${attachable.map((c) => `<option value="${esc(c.name)}">${esc(c.name)}</option>`).join("")}</select>
        <button id="btnAttachComp">+ Attach</button>
      </div>` : ""}
    </div>
    ${compsHtml || `<div class="card"><span class="slot-tag">No components attached.</span></div>`}
  `;

  $("btnDeleteEnt").addEventListener("click", async () => {
    const r = await api("DELETE", `/api/entity/${ent.slot}`);
    if (r.ok) { selectedSlot = null; await refresh(); }
  });
  $("btnSetParent").addEventListener("click", async () => {
    const p = $("selParent").value;
    if (!p) return;
    const r = await api("POST", `/api/entity/${ent.slot}/parent/${p}`);
    setInspectorStatus(r.ok ? "Parent updated" : (r.json && r.json.error) || "Error", r.ok);
    if (r.ok) await refresh();
  });
  $("btnClearParent").addEventListener("click", async () => {
    const r = await api("DELETE", `/api/entity/${ent.slot}/parent`);
    setInspectorStatus(r.ok ? "Parent cleared" : "Error", r.ok);
    if (r.ok) await refresh();
  });
  $("btnAddTag").addEventListener("click", async () => {
    const t = $("newTagInput").value.trim();
    if (!t) return;
    const r = await api("POST", `/api/entity/${ent.slot}/tag/${encodeURIComponent(t)}`);
    if (r.ok) await refresh();
  });
  $("inspectorBody").querySelectorAll("[data-rm-tag]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const t = btn.getAttribute("data-rm-tag");
      const r = await api("DELETE", `/api/entity/${ent.slot}/tag/${encodeURIComponent(t)}`);
      if (r.ok) await refresh();
    });
  });
  $("inspectorBody").querySelectorAll("[data-jump]").forEach((el) => {
    el.addEventListener("click", () => {
      selectedSlot = Number(el.getAttribute("data-jump"));
      renderTreeAndPrefabs();
      renderInspector();
    });
  });
  const btnAttach = $("btnAttachComp");
  if (btnAttach) {
    btnAttach.addEventListener("click", async () => {
      const cname = $("selAttachComp").value;
      const meta = metas.get(cname);
      const def = defaultForSchema(meta ? meta.schema : null);
      const r = await api("PUT", `/api/entity/${ent.slot}/component/${encodeURIComponent(cname)}`, def);
      setInspectorStatus(r.ok ? `Attached ${cname}` : (r.json && r.json.error) || "Error", r.ok);
      if (r.ok) await refresh();
    });
  }

  $("inspectorBody").querySelectorAll("[data-comp]").forEach((inp) => {
    inp.addEventListener("change", async () => {
      const cname = inp.getAttribute("data-comp");
      const fname = inp.getAttribute("data-field");
      const ftype = inp.getAttribute("data-ftype");
      let parsed;
      if (ftype === "bool") parsed = inp.checked;
      else if (ftype === "float") parsed = Number(inp.value);
      else if (ftype === "int") parsed = parseInt(inp.value, 10);
      else { try { parsed = JSON.parse(inp.value); } catch (_) { parsed = inp.value; } }

      const payload = fname ? { [fname]: parsed } : parsed;
      const r = await api("PATCH", `/api/entity/${ent.slot}/component/${encodeURIComponent(cname)}`, payload);
      const st = document.getElementById(`status-${cname}`);
      if (st) {
        st.textContent = r.ok ? "Saved" : ((r.json && r.json.error) || `HTTP ${r.status}`);
        st.className = `badge ${r.ok ? "ok" : "err"}`;
      }
      if (r.ok) {
        const idx = worldData.entities.findIndex((e) => e.slot === ent.slot);
        if (idx >= 0 && r.json) worldData.entities[idx] = r.json;
      }
    });
  });
}

function setInspectorStatus(msg, ok) {
  const el = $("inspectorStatus");
  el.textContent = msg;
  el.className = `badge ${ok ? "ok" : "err"}`;
}

function renderRightPane() {
  const body = $("rightPaneBody");
  if (activeTab === "archetypes") {
    const rows = (worldData.archetypes || []).map((a) => `
      <tr>
        <td class="slot-tag">#${a.id}</td>
        <td><strong>${a.entity_count}</strong></td>
        <td class="slot-tag">+${a.add_edges}/-${a.remove_edges}</td>
        <td><div class="chips">${(a.components || []).map((c) => {
          const cls = c.startsWith("#") ? "tag" : (c.startsWith("(") ? "prefab" : "comp");
          return `<span class="badge ${cls}">${esc(c)}</span>`;
        }).join("") || `<span class="slot-tag">[]</span>`}</div></td>
      </tr>
    `).join("");
    body.innerHTML = `
      <table class="grid">
        <thead><tr><th>Table</th><th>Ents</th><th>Edges</th><th>Signature</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    `;
  } else if (activeTab === "query") {
    const res = lastQuery.result;
    const resRows = res ? (res.entities || []).map((e) => `
      <tr class="clickable" data-qjump="${e.slot}">
        <td class="slot-tag">#${e.slot}</td>
        <td>${esc(entityLabel(e))}</td>
        <td class="slot-tag">${esc(Object.keys(e.components || {}).join(", "))}</td>
      </tr>
    `).join("") : "";
    body.innerHTML = `
      <div class="card">
        <div class="row" style="margin-bottom:6px">
          <input id="qWith" style="flex:1" placeholder="with (e.g. Pos,Vel)" value="${esc(lastQuery.with)}" />
        </div>
        <div class="row" style="margin-bottom:8px">
          <input id="qWithout" style="flex:1" placeholder="without (e.g. Dead)" value="${esc(lastQuery.without)}" />
        </div>
        <div class="row" style="justify-content:space-between">
          <label><input id="qPrefabs" type="checkbox" ${lastQuery.includePrefabs ? "checked" : ""} /> Include Prefabs</label>
          <button id="btnRunQuery" class="primary">Run Query</button>
        </div>
      </div>
      ${res ? `<div class="section-title"><span>Matches</span><span class="badge ok">${res.count}</span></div>
        <table class="grid"><thead><tr><th>Slot</th><th>Entity</th><th>Components</th></tr></thead><tbody>${resRows}</tbody></table>`
        : `<div class="empty-state">Enter comma-separated component or tag names and run a live query.</div>`}
    `;
    $("btnRunQuery").addEventListener("click", async () => {
      lastQuery.with = $("qWith").value.trim();
      lastQuery.without = $("qWithout").value.trim();
      lastQuery.includePrefabs = $("qPrefabs").checked;
      const params = new URLSearchParams();
      if (lastQuery.with) params.set("with", lastQuery.with);
      if (lastQuery.without) params.set("without", lastQuery.without);
      if (lastQuery.includePrefabs) params.set("include_prefabs", "true");
      const r = await api("GET", `/api/query?${params.toString()}`);
      if (r.ok && r.json) { lastQuery.result = r.json; renderRightPane(); }
    });
    body.querySelectorAll("[data-qjump]").forEach((tr) => {
      tr.addEventListener("click", () => {
        selectedSlot = Number(tr.getAttribute("data-qjump"));
        renderTreeAndPrefabs();
        renderInspector();
      });
    });
  } else {
    body.innerHTML = (worldData.components || []).map((c) => `
      <div class="card">
        <div class="card-head">
          <span>${esc(c.name)} <span class="slot-tag">#${c.id} &middot; ${c.size}B</span></span>
          <div class="row">
            ${c.can_copy ? `<span class="badge comp">Copy</span>` : ""}
            <span class="badge ${c.reflected ? "ok" : ""}">${c.reflected ? "Reflected" : "Opaque"}</span>
          </div>
        </div>
        <pre class="code">${esc(JSON.stringify(c.schema, null, 2))}</pre>
      </div>
    `).join("") || `<div class="empty-state">No components registered.</div>`;
  }
}

document.querySelectorAll(".tab-btn").forEach((btn) => {
  btn.addEventListener("click", () => {
    activeTab = btn.getAttribute("data-tab");
    document.querySelectorAll(".tab-btn").forEach((b) => b.classList.toggle("active", b === btn));
    renderRightPane();
  });
});

$("searchInput").addEventListener("input", renderTreeAndPrefabs);
$("btnRefresh").addEventListener("click", refresh);
$("btnHideBuiltins").addEventListener("click", () => {
  hideBuiltins = !hideBuiltins;
  $("btnHideBuiltins").classList.toggle("active", hideBuiltins);
  renderTreeAndPrefabs();
});
$("btnAuto").addEventListener("click", () => {
  if (autoTimer) {
    clearInterval(autoTimer); autoTimer = null;
    $("btnAuto").textContent = "Auto-Refresh: Off";
    $("btnAuto").classList.remove("active");
  } else {
    autoTimer = setInterval(refresh, 1000);
    $("btnAuto").textContent = "Auto-Refresh: 1s";
    $("btnAuto").classList.add("active");
  }
});

async function openExport(endpoint, title, filename) {
  const r = await api("GET", endpoint);
  const text = endpoint.endsWith(".tab") ? r.text : JSON.stringify(r.json || r.text, null, 2);
  lastExport = { name: filename, text };
  $("exportTitle").textContent = title;
  $("exportPre").textContent = text;
  $("exportModal").classList.add("open");
}
$("btnExportJson").addEventListener("click", () => openExport("/api/world", "World Snapshot (JSON)", "world.json"));
$("btnExportTab").addEventListener("click", () => openExport("/api/world.tab", "World Snapshot (TAB)", "world.tab"));
$("btnCloseExport").addEventListener("click", () => $("exportModal").classList.remove("open"));
$("btnCopyExport").addEventListener("click", () => navigator.clipboard && navigator.clipboard.writeText(lastExport.text));
$("btnDownloadExport").addEventListener("click", () => {
  const blob = new Blob([lastExport.text], { type: "text/plain;charset=utf-8" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = lastExport.name;
  a.click();
});

$("btnOpenSpawn").addEventListener("click", () => { $("spErr").textContent = ""; $("spawnModal").classList.add("open"); });
$("btnCloseSpawn").addEventListener("click", () => $("spawnModal").classList.remove("open"));
$("btnDoSpawn").addEventListener("click", async () => {
  $("spErr").textContent = "";
  const payload = {};
  if ($("spName").value.trim()) payload.name = $("spName").value.trim();
  if ($("spParent").value !== "") payload.parent = parseInt($("spParent").value, 10);
  if ($("spPrefab").value !== "") payload.prefab = parseInt($("spPrefab").value, 10);
  if ($("spTags").value.trim()) payload.tags = $("spTags").value.split(",").map((s) => s.trim()).filter(Boolean);
  if ($("spComps").value.trim()) {
    try { payload.components = JSON.parse($("spComps").value); }
    catch (e) { $("spErr").textContent = "Invalid components JSON"; return; }
  }
  const r = await api("POST", "/api/entity", payload);
  if (!r.ok) {
    $("spErr").textContent = (r.json && r.json.error) || `HTTP ${r.status}`;
    return;
  }
  $("spawnModal").classList.remove("open");
  $("spName").value = ""; $("spParent").value = ""; $("spPrefab").value = ""; $("spTags").value = ""; $("spComps").value = "";
  if (r.json) selectedSlot = r.json.slot;
  await refresh();
});

refresh();
</script>
</body>
</html>
"##;
