//! A unit's keys from its file images (D39, docs/v2/plan/design/read-addressed.md): [`asks`] for
//! the late reads to answer, [`key`] for every key.
//!
//! A key says what a record is and reads nothing of any other unit:
//! - each body: `BodyInputs { tool, settings, unit, item, text }`, the item's own tokens;
//! - the surface: `SurfaceInputs { tool, settings, unit, pieces }`, one piece per active node in
//!   the order of the nodes' stable ids: the node's stable id, its header digest, the digests of
//!   what escapes its bodies (its own and its associated items'), and one flag, whether a body
//!   of it holds a macro that is not quiet;
//! - the manifest: `UnitInputs { tool, settings, unit, files, late }`, the file images' keys in
//!   the unit's file order and every late read's answer.
//!
//! A late read is in the manifest's key and nowhere else: the surface or body that made it asks
//! for it like any fact. An item under a `#[cfg]` that is off is no item of the unit: no piece
//! and no body key; `settings` covers the cfg options (rule 21: the caller's to keep).
//!
//! # Floors (rule 67)
//!
//! The floor of keying a unit after one file changed is that file's one parse (the image) and
//! one pass over the unit's item rows; [`key`] costs exactly that: every unchanged file's image
//! is opened in place (O(sections)), and each row costs one cfg walk of its own `#[cfg]`, one
//! hash of its path (its stable id), one hash for its key, and its share of two sorts (body
//! items by file, pieces across the unit): O(rows log rows) hashes of 32-byte values, no token
//! read. What moves:
//! - a comment or whitespace edit: one file reparsed; that file's image key and the manifest key
//!   move, nothing else (no digest holds a comment or whitespace, except between two
//!   punctuation tokens, where `- -` and `--` are different token streams);
//! - a body edit: that, and the one body key whose item's text it is in;
//! - a header edit: that, the edited item's body key (its text holds its header) and the
//!   surface key.

#![blaze::sans_io]

use core::{alloc::Allocator, cmp::Ordering};
use std::alloc::Global;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{
        Body, BodyInputs, FileImage, Key, Kind, Late, LateRead, Settings, Surface, SurfaceInputs, ToolId, UnitInputs,
        UnitManifest,
    },
    record::StableId,
};
use cfg::CfgOptions;

use crate::{
    KeyError, cfgs, quiet,
    read::{Image, OutOfPool, stable_of},
    rows::{BODY, CALLS, ItemRow, LateRow, NODE, NONE},
};

/// One file of the unit: its image's key, the image (16-aligned), its module path (`` for the
/// crate root, `a::b`).
#[derive(Clone, Copy)]
pub struct UnitFile<'a> {
    pub key: Key<FileImage>,
    pub image: &'a [u8],
    pub module: &'a [u8],
}

/// The caller's answer to one late read: what file `file`'s `late` of `name` read (`None`:
/// absent, which is a value). Sorted by (file, late, name), each once.
#[derive(Clone, Copy, Debug)]
pub struct LateAnswer<'a> {
    pub file: u32,
    pub late: Late,
    pub name: &'a [u8],
    pub value: Option<ContentId>,
}

/// Everything a unit's keys are a function of, and nothing of another unit.
#[derive(Clone, Copy)]
pub struct Unit<'a> {
    pub tool: ToolId,
    /// The unit's typed settings; they cover `cfg`.
    pub settings: Settings,
    /// The unit's identity (name, version, source): two units of equal text key apart.
    pub unit: ContentId,
    /// The unit's cfg: each row's `#[cfg]` is decided under it.
    pub cfg: &'a CfgOptions,
    /// The unit's files, in the unit's file order (the crate root first, then each module file
    /// as the caller found it).
    pub files: &'a [UnitFile<'a>],
    pub answers: &'a [LateAnswer<'a>],
}

/// One late read to answer before keying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ask<'a> {
    pub file: u32,
    pub late: Late,
    /// The variable or path; when `literal` is false, the argument's token text (`concat!(..)`),
    /// which the caller evaluates first.
    pub name: &'a [u8],
    pub literal: bool,
}

/// One file's part of a unit's keys: its image's key, and its items' body keys as a run of
/// [`UnitKeys::items`]. The driver's own type (`blaze_keyed::front::FileItems`), so the adapter
/// moves a unit's files and items into `front::Keyed` without a copy.
pub use blaze_keyed::front::FileItems as FileKeys;

/// Every key of a unit: the parts of `blaze_keyed::front::Keyed`. The manifest's key, the
/// surface's key, and per file in the unit's file order its image's key and its items
/// `(StableId, Key<Body>)` in the order of their stable ids (a manifest is chunked by file).
pub struct UnitKeys<A: Allocator> {
    pub manifest: Key<UnitManifest>,
    pub surface: Key<Surface>,
    pub files: Vec<FileKeys, A>,
    /// Every active item with a body and its key: file by file, each file's run sorted by
    /// stable id, each item once.
    pub items: Vec<(StableId, Key<Body>), A>,
}

impl<A: Allocator> UnitKeys<A> {
    /// File `file`'s items with their body keys, sorted by stable id.
    pub fn file_items(&self, file: usize) -> &[(StableId, Key<Body>)] {
        let keys = &self.files[file];
        &self.items[keys.at as usize..(keys.at + keys.len) as usize]
    }
}

/// The late reads of the unit's active items, sorted by (file, late, name), each once.
///
/// Cost: O(rows + late reads) after opening each image. Floor: the same.
pub fn asks<'a, A: Allocator + Clone>(unit: &Unit<'a>, frame: A) -> Result<Vec<Ask<'a>, A>, KeyError> {
    let mut out = Vec::new_in(frame.clone());
    let mut alive = Vec::new_in(frame);
    for (file, entry) in unit.files.iter().enumerate() {
        let file = file as u32;
        let image = Image::open(entry.image, &entry.key).map_err(|error| KeyError::Image { file, error })?;
        let pool = |error| KeyError::Pool { file, error };
        active(&image, unit.cfg, &mut alive).map_err(pool)?;
        for (at, row) in image.items().iter().enumerate() {
            if !alive[at] {
                continue;
            }
            for late in image.lates(row.lates).map_err(pool)? {
                let name = image.str(late.name).map_err(pool)?;
                out.push(Ask { file, late: late_of(late, file)?, name, literal: late.literal == 1 });
            }
        }
    }
    out.sort_by(|a, b| (a.file, a.late as u8, a.name).cmp(&(b.file, b.late as u8, b.name)));
    out.dedup();
    Ok(out)
}

/// Every row's cfg decided at once, into `alive` (cleared): a row is active when the file's
/// `#![cfg]`, its own `#[cfg]` and its parent's activity hold. Rows are in preorder, so a
/// parent is decided before its child; a parent at or after its child is refused.
///
/// Cost: one walk of each row's own predicate. Floor: the same.
pub fn active<A: Allocator>(image: &Image<'_>, options: &CfgOptions, alive: &mut Vec<bool, A>) -> Result<(), OutOfPool> {
    const UNDECODABLE: &str = "a `#[cfg]` that does not decode";
    alive.clear();
    let file = cfgs::holds(image.file_cfg()?, options).map_err(|_| OutOfPool { pool: UNDECODABLE, at: 0 })?;
    for (at, row) in image.items().iter().enumerate() {
        let at = at as u32;
        let parent = match row.parent {
            NONE => file,
            parent if parent < at => alive[parent as usize],
            _ => return Err(OutOfPool { pool: "a parent row after its child", at }),
        };
        let own = parent && cfgs::holds(image.cfg(row.cfg)?, options).map_err(|_| OutOfPool { pool: UNDECODABLE, at })?;
        alive.push(own);
    }
    Ok(())
}

fn late_of(late: &LateRow, file: u32) -> Result<Late, KeyError> {
    quiet::late_from(late.late).ok_or(KeyError::Pool { file, error: OutOfPool { pool: "a late read's macro", at: late.token } })
}

/// Every key of `unit`. Reads its images in place and nothing else: no record of another unit,
/// no store, no closure.
///
/// Cost and floor: in the module's doc. Scratch is in `frame`.
pub fn key<'a, A: Allocator + Clone>(unit: &Unit<'a>, frame: A) -> Result<UnitKeys<A>, KeyError> {
    check_answers(unit.answers)?;
    let mut files = Vec::with_capacity_in(unit.files.len(), frame.clone());
    let mut items: Vec<(StableId, Key<Body>), A> = Vec::new_in(frame.clone());
    let mut pieces: Vec<(StableId, ContentId), A> = Vec::new_in(frame.clone());
    let mut lates: Vec<LateRead<'a>, A> = Vec::new_in(frame.clone());
    let mut alive = Vec::new_in(frame.clone());
    let mut file_keys = Vec::with_capacity_in(unit.files.len(), frame.clone());
    for (file, entry) in unit.files.iter().enumerate() {
        let file = file as u32;
        let image = Image::open(entry.image, &entry.key).map_err(|error| KeyError::Image { file, error })?;
        let pool = |error| KeyError::Pool { file, error };
        active(&image, unit.cfg, &mut alive).map_err(pool)?;
        file_keys.push(entry.key);
        let first = items.len();
        // The node whose piece is open: its stable id, its row, and its piece's hasher.
        let mut open: Option<(StableId, u32, Hasher)> = None;
        let rows = image.items();
        for (at, row) in rows.iter().enumerate() {
            let at = at as u32;
            if row.flags & NODE != 0 {
                if let Some((stable, _, hasher)) = open.take() {
                    pieces.push((stable, hasher.finish()));
                }
            } else {
                // An associated row follows its node and the node's other associated rows.
                match &open {
                    Some((_, node, _)) if row.parent == *node => {}
                    // Its node is off, so it is too.
                    None if !alive[at as usize] => continue,
                    _ => return Err(pool(OutOfPool { pool: "an associated row apart from its node", at })),
                }
            }
            if !alive[at as usize] {
                continue;
            }
            let path = image.str(row.path).map_err(pool)?;
            let stable = stable_of(entry.module, path, row.kind);
            if row.flags & NODE != 0 {
                let mut hasher = Hasher::new("blaze-unitkey piece");
                hasher.id(&stable.0).id(&row.header);
                open = Some((stable, at, hasher));
            }
            if let Some((_, _, hasher)) = &mut open {
                piece_part(hasher, &image, row).map_err(pool)?;
            }
            if row.flags & BODY != 0 {
                let key = Body::key(&BodyInputs {
                    tool: unit.tool,
                    settings: unit.settings,
                    unit: unit.unit,
                    item: stable,
                    text: row.text,
                });
                items.push((stable, key));
            }
            answer(unit, &image, file, row, &mut lates)?;
        }
        if let Some((stable, _, hasher)) = open.take() {
            pieces.push((stable, hasher.finish()));
        }
        items[first..].sort_unstable_by_key(|(stable, _)| *stable);
        if let Some(pair) = items[first..].windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(two_items(&image, entry.module, file, pair[0].0));
        }
        let at = u32::try_from(first).map_err(|_| KeyError::TooLarge { what: "a unit's items" })?;
        let len = u32::try_from(items.len() - first).map_err(|_| KeyError::TooLarge { what: "a file's items" })?;
        files.push(FileKeys { image: entry.key, at, len });
    }

    // The surface: the pieces in the order of the nodes' stable ids across the unit.
    pieces.sort_unstable_by_key(|(stable, _)| *stable);
    if let Some(pair) = pieces.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(KeyError::TwoItems { file: None, stable: pair[0].0, path: Vec::new_in(Global) });
    }
    let mut ids = Vec::with_capacity_in(pieces.len(), frame.clone());
    ids.extend(pieces.iter().map(|(_, piece)| *piece));
    let surface = Surface::key(&SurfaceInputs { tool: unit.tool, settings: unit.settings, unit: unit.unit, pieces: &ids });

    // The manifest: the file images in the unit's order, every late read's answer.
    sort_lates(&mut lates);
    let manifest = UnitManifest::key(&UnitInputs {
        tool: unit.tool,
        settings: unit.settings,
        unit: unit.unit,
        files: &file_keys,
        late: &lates,
    });
    Ok(UnitKeys { manifest, surface, files, items })
}

/// A row's share of its node's piece: the digests of what escapes its body, and its flag.
fn piece_part(hasher: &mut Hasher, image: &Image<'_>, row: &ItemRow) -> Result<(), OutOfPool> {
    let escapes = image.escapes(row.escapes)?;
    hasher.update(&(escapes.len() as u64).to_le_bytes());
    for escape in escapes {
        hasher.id(escape);
    }
    hasher.update(&[(row.flags & CALLS != 0) as u8]);
    Ok(())
}

/// Two items of one stable id, refused by name (rule 21: one key, one value).
fn two_items(image: &Image<'_>, module: &[u8], file: u32, stable: StableId) -> KeyError {
    let mut path = Vec::new_in(Global);
    for at in 0..image.items().len() as u32 {
        if image.stable_id(module, at).is_ok_and(|id| id == stable) {
            path.extend_from_slice(module);
            if !module.is_empty() {
                path.extend_from_slice(b"::");
            }
            path.extend_from_slice(image.str(image.items()[at as usize].path).unwrap_or(&[]));
            break;
        }
    }
    KeyError::TwoItems { file: Some(file), stable, path }
}

/// The answers to `row`'s late reads (file `file`'s), appended to `out`.
fn answer<'a, A: Allocator>(
    unit: &Unit<'a>,
    image: &Image<'_>,
    file: u32,
    row: &ItemRow,
    out: &mut Vec<LateRead<'a>, A>,
) -> Result<(), KeyError> {
    let pool = |error| KeyError::Pool { file, error };
    for late_row in image.lates(row.lates).map_err(pool)? {
        let late = late_of(late_row, file)?;
        let name = image.str(late_row.name).map_err(pool)?;
        let at = unit.answers.binary_search_by(|answer| answer_order(answer, file, late, name)).map_err(|_| {
            let mut owned = Vec::new_in(Global);
            owned.extend_from_slice(name);
            KeyError::Unanswered { file, late: late as u8, name: owned }
        })?;
        let answer = &unit.answers[at];
        out.push(LateRead { late, name: answer.name, value: answer.value });
    }
    Ok(())
}

fn check_answers(answers: &[LateAnswer<'_>]) -> Result<(), KeyError> {
    for pair in answers.windows(2) {
        if answer_order(&pair[0], pair[1].file, pair[1].late, pair[1].name) != Ordering::Less {
            return Err(KeyError::Answers);
        }
    }
    Ok(())
}

fn answer_order(answer: &LateAnswer<'_>, file: u32, late: Late, name: &[u8]) -> Ordering {
    (answer.file, answer.late as u8, answer.name).cmp(&(file, late as u8, name))
}

/// Sorts late reads by (late, name, value), each once: two files' `include_str!("a.txt")` read
/// different files, so a read is the same read only when its value is too.
fn sort_lates(lates: &mut Vec<LateRead<'_>, impl Allocator>) {
    lates.sort_by(|a, b| (a.late as u8, a.name, a.value).cmp(&(b.late as u8, b.name, b.value)));
    lates.dedup_by(|a, b| a.late == b.late && a.name == b.name && a.value == b.value);
}
