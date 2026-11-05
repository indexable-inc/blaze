//! A world from a flecs-snapshot document (flecs-opinionated SPEC.md, "Document"): DAG-CBOR
//! `{version, layouts, entities}`, the entities depth first, each with its path, parent index, name, tags, pairs,
//! component values and prefabs. References to other entities are `{"id": bytes}` (a stable id) or `{"path": text}`
//! (Flecs's builtins); they are resolved to paths here, so the snapshot has to hold every entity the site's entities
//! refer to (the build passes every root, `World::roots`).
//!
//! The reader is strict, as flecs-snapshot's own (`check::document`) is: a document or entity record with a key the
//! spec does not list is refused, since within version 1 no key may be added. It reads only scalar fields; `sync.Bytes`
//! and `sync.Blob` values are refused (the site has none). Layout records (with their `"sync"` mode) are not read:
//! the renderer reads values by field name.
//!
//! Two things a snapshot leaves out that the renderer needs, and how they come back:
//! - Pair order. A snapshot sorts an entity's pairs by their encoding (stable ids are hashes); Flecs keeps no declared
//!   order either (SPEC "Entity record"). The targets of one relationship are put back in document order, which for
//!   targets declared in script order (refs under OrderedChildren, pages) is the order Flecs created them in, so
//!   `(Cites, a); (Cites, b)` numbers its citations as the TS renderer does.
//! - Unordered children. A snapshot sorts the children of a parent without OrderedChildren by name (SPEC "Document").
//!   Every container in the site's schema has OrderedChildren, and RFC lists sort by `Number` (`World::rfcs`), so this
//!   shows only for an entity with several children that is not a container.

use std::collections::HashMap;

use ipld_core::ipld::Ipld;

use crate::world::{Entity, Scalar, Value, World};

const BUILTIN: &str = "flecs.core.";

fn field<'a>(m: &'a Ipld, k: &str) -> Option<&'a Ipld> {
    match m {
        Ipld::Map(m) => m.get(k),
        _ => None,
    }
}

/// `m` is a map whose keys are all in `allowed` (SPEC.md, "Versioning": readers are strict).
fn keys(m: &Ipld, allowed: &[&str], what: &str) -> Result<(), String> {
    let Ipld::Map(m) = m else { return Err(format!("snapshot: {what} is not a map")) };
    match m.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("snapshot: {what}: unknown key {k:?}")),
        None => Ok(()),
    }
}

const DOCUMENT_KEYS: &[&str] = &["version", "layouts", "entities"];
const ENTITY_KEYS: &[&str] =
    &["id", "name", "tags", "pairs", "components", "isa", "inherited", "path", "parent"];
const COMPONENT_KEYS: &[&str] = &["hash", "value"];

fn list(v: Option<&Ipld>) -> &[Ipld] {
    match v {
        Some(Ipld::List(l)) => l,
        _ => &[],
    }
}

fn string(v: Option<&Ipld>) -> Option<&str> {
    match v {
        Some(Ipld::String(s)) => Some(s),
        _ => None,
    }
}

fn short(path: &str) -> &str {
    path.strip_prefix(BUILTIN).unwrap_or(path)
}

/// The path segment, unescaped (`\.` is a dot in a name).
fn unescape(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let mut it = seg.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The last segment of an escaped path.
fn last_segment(path: &str) -> &str {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        if b[i] == b'.' {
            let mut slashes = 0;
            while i > slashes && b[i - 1 - slashes] == b'\\' {
                slashes += 1;
            }
            if slashes % 2 == 0 {
                return &path[i + 1..];
            }
        }
    }
    path
}

fn scalar(v: &Ipld, at: &str) -> Result<Scalar, String> {
    Ok(match v {
        Ipld::Null => Scalar::Null,
        Ipld::Bool(b) => Scalar::Bool(*b),
        Ipld::Integer(i) => Scalar::Int(
            i64::try_from(*i).map_err(|_| format!("{at}: integer {i} does not fit i64"))?,
        ),
        Ipld::Float(f) => Scalar::Float(*f),
        Ipld::String(s) => Scalar::Str(s.clone()),
        Ipld::Bytes(_) | Ipld::Link(_) => {
            return Err(format!("{at}: bytes and blobs are not read by the renderer"));
        }
        _ => return Err(format!("{at}: only scalar fields are supported")),
    })
}

fn value(v: &Ipld, at: &str) -> Result<Value, String> {
    match v {
        Ipld::Map(m) => {
            m.iter().map(|(k, f)| Ok((k.clone(), scalar(f, &format!("{at}.{k}"))?))).collect()
        }
        _ => Err(format!("{at}: a component value is a struct")),
    }
}

/// Paths of escaped segments become dotted paths of names, as the TS tree writes them.
fn plain_path(path: &str) -> String {
    let mut segs = Vec::new();
    let mut rest = path;
    loop {
        let seg = last_segment(rest);
        segs.push(unescape(seg));
        if seg.len() == rest.len() {
            break;
        }
        rest = &rest[..rest.len() - seg.len() - 1];
    }
    segs.reverse();
    segs.join(".")
}

/// The world in a snapshot document (DAG-CBOR bytes).
pub fn load(bytes: &[u8]) -> Result<World, String> {
    let doc: Ipld = serde_ipld_dagcbor::from_slice(bytes)
        .map_err(|e| format!("snapshot: not DAG-CBOR: {e}"))?;
    from_document(&doc)
}

pub fn from_document(doc: &Ipld) -> Result<World, String> {
    match field(doc, "version") {
        Some(Ipld::Integer(1)) => {}
        v => return Err(format!("snapshot: version {v:?}, expected 1")),
    }
    keys(doc, DOCUMENT_KEYS, "the document")?;
    let entities = list(field(doc, "entities"));
    // Stable id to path, and path to document position, over every entity.
    let mut by_id: HashMap<&[u8], String> = HashMap::new();
    let mut order: HashMap<String, usize> = HashMap::new();
    let mut paths = Vec::with_capacity(entities.len());
    for (i, e) in entities.iter().enumerate() {
        keys(e, ENTITY_KEYS, &format!("entity {i}"))?;
        let p =
            string(field(e, "path")).ok_or_else(|| format!("snapshot: entity {i} has no path"))?;
        let p = plain_path(p);
        if let Some(Ipld::Bytes(id)) = field(e, "id") {
            by_id.insert(id, p.clone());
        }
        order.insert(p.clone(), i);
        paths.push(p);
    }
    let resolve = |r: &Ipld| -> Result<String, String> {
        if let Some(Ipld::Bytes(id)) = field(r, "id") {
            return by_id.get(id.as_slice()).cloned().ok_or_else(|| {
                "refers to an entity outside the snapshot (pass every root)".into()
            });
        }
        string(field(r, "path")).map(plain_path).ok_or_else(|| format!("bad reference {r:?}"))
    };

    let mut world = World::default();
    for (i, e) in entities.iter().enumerate() {
        let path = &paths[i];
        let at = |m: String| format!("snapshot: {path}: {m}");
        let parent = match field(e, "parent") {
            Some(Ipld::Integer(p)) => Some(
                usize::try_from(*p)
                    .ok()
                    .filter(|p| *p < i)
                    .ok_or_else(|| at("bad parent".into()))?,
            ),
            _ => None,
        };
        let name = match string(field(e, "name")) {
            Some(n) => n.to_string(),
            None => unescape(last_segment(string(field(e, "path")).unwrap_or_default())),
        };
        let mut ent = Entity { name, ..Entity::default() };
        for t in list(field(e, "tags")) {
            ent.tags.push(short(&resolve(t).map_err(at)?).to_string());
        }
        for p in list(field(e, "pairs")) {
            let parts = list(Some(p));
            let [r, t, ..] = parts else { return Err(at("bad pair".into())) };
            let rel = short(&resolve(r).map_err(at)?).to_string();
            if rel == "IsA" {
                continue;
            }
            let tgt = resolve(t).map_err(at)?;
            match ent.pairs.iter_mut().find(|(x, _)| *x == rel) {
                Some((_, ts)) => ts.push(tgt),
                None => ent.pairs.push((rel, vec![tgt])),
            }
        }
        for (_, ts) in &mut ent.pairs {
            ts.sort_by_key(|t| order.get(t).copied().unwrap_or(usize::MAX));
        }
        if let Some(Ipld::Map(cs)) = field(e, "components") {
            for (k, c) in cs {
                keys(c, COMPONENT_KEYS, &format!("{path}: component {k}"))?;
                let v =
                    field(c, "value").ok_or_else(|| at(format!("component {k} has no value")))?;
                ent.values.push((plain_path(k), value(v, &format!("{path}: {k}"))?));
            }
        }
        for b in list(field(e, "isa")) {
            ent.isa.push(resolve(b).map_err(at)?);
        }
        let id = world.spawn(parent, ent).map_err(at)?;
        if world.get(id).path != *path {
            return Err(at(format!("path does not match its parent's ({})", world.get(id).path)));
        }
    }
    Ok(world)
}
