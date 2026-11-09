//! A field order plan's file (docs/v2/plan/design/field-order.md): each ADT's hot
//! fields as `hir_ty::layout::field_order::heat` ranked them, the ADT named by its item key so
//! the plan one profile gives is read by every build of the tree. Text, sorted by key:
//!
//! ```text
//! blaze v2 field order 1
//! adt <the ADT's item key, 64 hex digits> <how many variants it has>
//! v <variant index> <hot field name, hottest first>...
//! ```
//!
//! A variant with no hot field has no `v` line. The plan's ID ([`FieldOrderPlan::id`]) is the
//! content ID of the file's bytes.

use std::{alloc::Global, fmt, fmt::Write as _};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::AdtId;
use hir_expand::name::Name;
use hir_ty::layout::field_order::{self, AdtPlan, FieldOrderPlan, Stale};

use crate::{
    ItemId, ItemKey,
    key::{self, Resolved},
};

/// The file's first line.
pub const HEADER: &str = "blaze v2 field order 1";

/// The domain of a plan's ID.
const PLAN_DOMAIN: &str = "blaze v2 field order plan 1";

/// The plan file of `plan` (as `Heat::ranked` gives it).
pub fn encode(db: &Db, plan: &[(AdtId, AdtPlan)]) -> Vec<u8> {
    // alloc: once per plan
    let mut keyed: Vec<(ItemKey, &AdtPlan)> =
        plan.iter().map(|(adt, it)| (key::item_key(db, ItemId::Adt(*adt)), it)).collect();
    keyed.sort_by_key(|&(key, _)| key);
    // alloc: the file
    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    for (key, it) in keyed {
        _ = writeln!(out, "adt {:x} {}", key.0, it.variants.len());
        for (v, names) in it.variants.iter().enumerate() {
            if names.is_empty() {
                continue;
            }
            _ = write!(out, "v {v}");
            for name in names.iter() {
                _ = write!(out, " {}", name.as_str());
            }
            out.push('\n');
        }
    }
    out.into_bytes()
}

/// A plan file read against the current database.
#[derive(Debug)]
pub struct Decoded {
    pub plan: FieldOrderPlan,
    /// ADTs the file names that this database has no item for (renamed, moved, or of a crate
    /// not loaded): left out of the plan, so they keep rustc's order.
    pub unresolved: u32,
    /// ADTs whose ranks don't fit their fields here (a field renamed or removed since the
    /// profile, or a variant added), in the file's order (by item key), each with why: they
    /// keep rustc's order. The driver reports these and `unresolved` with the build, as it
    /// reports functions whose profile records don't match.
    pub stale: Vec<(AdtId, Stale)>,
}

/// Why a plan file was refused: the line (1-based) and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub line: usize,
    pub what: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "field order plan, line {}: {}", self.line, self.what)
    }
}

impl std::error::Error for DecodeError {}

fn refuse(line: usize, what: impl Into<String>) -> DecodeError {
    DecodeError { line, what: what.into() }
}

/// Reads a plan file, resolving its ADTs' keys in `db`. A malformed line, an item that is no
/// ADT, or an ADT named twice is refused, naming the line.
pub fn decode(db: &Db, bytes: &[u8]) -> Result<Decoded, DecodeError> {
    let text = std::str::from_utf8(bytes).map_err(|err| refuse(0, format!("not UTF-8: {err}")))?;
    let mut lines = text.lines().enumerate().map(|(at, line)| (at + 1, line));
    match lines.next() {
        Some((_, HEADER)) => {}
        other => {
            return Err(refuse(
                1,
                format!("expected `{HEADER}`, found {:?}", other.map(|it| it.1)),
            ));
        }
    }
    let mut by_adt: HashMap<AdtId, AdtPlan, Global> = HashMap::new_in(Global);
    let mut unresolved = 0;
    // alloc: the decoded plan's, one entry per stale ADT
    let mut stale = Vec::new();
    // The ADT being read (`None`: not in this database), its line and its variants.
    let mut current: Option<(Option<AdtId>, usize, Vec<Box<[Name]>>)> = None;
    let mut finish = |current: Option<(Option<AdtId>, usize, Vec<Box<[Name]>>)>| {
        let Some((Some(adt), line, variants)) = current else { return Ok(()) };
        let plan = AdtPlan { variants: variants.into_boxed_slice() };
        if let Some(why) = field_order::stale(db, adt, &plan) {
            stale.push((adt, why));
        }
        match by_adt.insert(adt, plan) {
            None => Ok(()),
            Some(_) => Err(refuse(line, "names an ADT a line before names too")),
        }
    };
    for (line, text) in lines {
        let mut words = text.split(' ');
        match words.next() {
            Some("adt") => {
                finish(current.take())?;
                let usage = "expected `adt <64 hex digits> <variants>`";
                let key = words.next().and_then(parse_key).ok_or_else(|| refuse(line, usage))?;
                let count: usize = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .ok_or_else(|| refuse(line, usage))?;
                if words.next().is_some() {
                    return Err(refuse(line, usage));
                }
                let adt = match key::resolve(db, ItemKey(key)) {
                    Some(Resolved::Item(ItemId::Adt(adt))) => Some(adt),
                    Some(other) => {
                        return Err(refuse(
                            line,
                            format!("key {key:x} names {other:?}, not a struct, enum or union"),
                        ));
                    }
                    None => {
                        unresolved += 1;
                        None
                    }
                };
                // alloc: the plan's
                current = Some((adt, line, vec![Box::default(); count]));
            }
            Some("v") => {
                let Some((_, _, variants)) = &mut current else {
                    return Err(refuse(line, "a `v` line before any `adt` line"));
                };
                let usage = "expected `v <variant index> <field name>...`";
                let index: usize = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .ok_or_else(|| refuse(line, usage))?;
                let count = variants.len();
                let Some(slot) = variants.get_mut(index) else {
                    return Err(refuse(line, format!("variant {index} of an ADT of {count}")));
                };
                if !slot.is_empty() {
                    return Err(refuse(line, format!("variant {index} ranked a line before")));
                }
                // alloc: the plan's
                let names: Box<[Name]> = words.map(Name::new_root).collect();
                if names.is_empty() {
                    return Err(refuse(line, usage));
                }
                *slot = names;
            }
            _ => {
                return Err(refuse(line, format!("expected an `adt` or `v` line, found `{text}`")));
            }
        }
    }
    finish(current.take())?;
    let plan = FieldOrderPlan::new(ContentId::of(PLAN_DOMAIN, bytes), by_adt);
    Ok(Decoded { plan, unresolved, stale })
}

/// 64 lowercase hex digits.
fn parse_key(hex: &str) -> Option<ContentId> {
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0; 32];
    for (at, pair) in hex.chunks_exact(2).enumerate() {
        bytes[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(ContentId(bytes))
}
