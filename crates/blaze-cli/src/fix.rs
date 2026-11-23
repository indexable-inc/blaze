//! Every finding has an id, two plain views and a fix applied by id (decision D70).
//!
//! THE ID is derived from what the finding is, never from where it prints: a digest of the item
//! it sits in (its file, its chain in the file, its text), its code and its range relative to the
//! item (rule 71), shown as a prefix in [`ALPHABET`] (lowercase, no `0 1 i l o`). Each id is
//! [`SHORT`] characters, longer only where two findings of one run share a prefix: then each of
//! the two is one past the prefix they share ([`lengths`]), so no shown id is a prefix of another
//! finding's. An edit elsewhere in the file moves the item, never its text or the range in it:
//! the id stands. An edit of the item changes it. A Rust finding's item is the innermost item of
//! the file's outline holding it; a finding in no item (a manifest's, or one between items) is
//! keyed by its line, its "item".
//!
//! `blaze check --fix <selector>...` ([`apply`]): a selector is `all`, a code (`E0425`), a path,
//! or an id or any unique prefix of one. The ids are made from today's text by the check the same
//! command just ran, so a finding whose text changed since an id was printed has another id: its
//! old id matches nothing and is refused by name. Fixes are one transaction: every refusal (no
//! match, an ambiguous prefix, a finding with no fix, two fixes that overlap, a file changed
//! between this command's check and its write) is decided before the first byte is written. Each
//! file is then replaced whole through the effect layer's durable publish (a new file synced,
//! renamed over the old, the directory synced): a crash leaves each file old or new, never a part.

use blaze_drive::Name;
use blaze_own::{Alloc, Vec};
use blaze_report::frame::words::{Area, HelpKind};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;

/// The id's alphabet: digits and lowercase letters without the look-alikes `0 1 i l o`.
pub(crate) const ALPHABET: &[u8; 31] = b"23456789abcdefghjkmnpqrstuvwxyz";
/// The shown length of an id no other finding of the run shares a prefix with.
pub(crate) const SHORT: usize = 5;
/// The whole id: 25 characters of the alphabet hold 123 bits of the digest.
pub(crate) const FULL: usize = 25;
const ID_DOMAIN: &str = "blaze-cli finding id 1";

/// A finding's id: its whole digest in [`ALPHABET`], and how much of it is shown.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Id {
    full: [u8; FULL],
    shown: u8,
}

impl Id {
    /// The id of a finding: `about` its file (or, for one of no file, what it is about), `chain`
    /// its item's chain in the file, `item` the item's text, `code` the rule's name, and its range
    /// relative to the item's start.
    pub(crate) fn of(about: &[u8], chain: u64, item: &[u8], code: &str, (start, end): (u32, u32)) -> Id {
        let mut h = blaze_ids::Hasher::new(ID_DOMAIN);
        h.update(&(about.len() as u32).to_le_bytes()).update(about).update(&chain.to_le_bytes());
        h.update(&(item.len() as u32).to_le_bytes()).update(item);
        h.update(&(code.len() as u32).to_le_bytes()).update(code.as_bytes());
        h.update(&start.to_le_bytes()).update(&end.to_le_bytes());
        let digest = h.finish();
        let mut n = 0u128;
        for &byte in digest.0[..16].iter() {
            n = n << 8 | u128::from(byte);
        }
        let mut full = [0u8; FULL];
        for c in full.iter_mut() {
            *c = ALPHABET[(n % ALPHABET.len() as u128) as usize];
            n /= ALPHABET.len() as u128;
        }
        Id { full, shown: SHORT as u8 }
    }

    /// The id of a finding never drawn: one only counted under its cause (print.rs
    /// `Place::Counted`, never under `--each`). No digest is made for it (tempo's no-op made
    /// 31,797, one a finding, to draw 753 lines); it shows as nothing, no selector's prefix
    /// matches it, and [`lengths`] leaves it out.
    pub(crate) const UNSHOWN: Id = Id { full: [0; FULL], shown: 0 };

    /// Whether this id is a drawn finding's ([`Id::UNSHOWN`] is not).
    pub(crate) fn is_shown(&self) -> bool {
        self.shown != 0
    }

    /// The id as shown.
    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.full[..usize::from(self.shown)]).unwrap_or("")
    }

    /// The whole id and its shown length: what a stored file section keeps (engine_lines.rs BER2).
    pub(crate) fn parts(&self) -> ([u8; FULL], u8) {
        (self.full, self.shown)
    }

    /// The id [`Id::parts`] gave; `None` when a byte is outside [`ALPHABET`] or the shown length is outside `SHORT..=FULL`.
    pub(crate) fn of_parts(full: [u8; FULL], shown: u8) -> Option<Id> {
        (full.iter().all(|byte| ALPHABET.contains(byte)) && (SHORT..=FULL).contains(&usize::from(shown))).then_some(Id { full, shown })
    }

    /// Whether `prefix` begins this id.
    fn starts(&self, prefix: &str) -> bool {
        !prefix.is_empty() && self.full.starts_with(prefix.as_bytes())
    }

    fn same(&self, other: &Id) -> bool {
        self.full == other.full
    }

    /// An id whose whole text is `text` (padded with `2`): a test's, to place ids side by side.
    #[cfg(test)]
    pub(crate) fn of_text(text: &str) -> Id {
        let mut full = [b'2'; FULL];
        full[..text.len()].copy_from_slice(text.as_bytes());
        Id { full, shown: SHORT as u8 }
    }
}

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Each id's shown length (D70's growth rule): [`SHORT`], or one past the longest prefix it
/// shares with another finding's id. Findings with the same whole id are one finding printed
/// twice (one per configuration): they share their length.
pub(crate) fn lengths(listed: &mut [Listed<'_>], frame: &Frame) {
    let mut order: Vec<usize, &Frame> = Vec::with_capacity_in(listed.len(), frame);
    order.extend((0..listed.len()).filter(|&at| listed[at].id.is_shown()));
    order.sort_unstable_by(|&a, &b| listed[a].id.full.cmp(&listed[b].id.full));
    for pair in order.windows(2) {
        let (a, b) = (listed[pair[0]].id.full, listed[pair[1]].id.full);
        if a == b {
            continue;
        }
        let shared = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
        let need = (shared + 1).max(SHORT) as u8;
        for &at in pair {
            listed[at].id.shown = listed[at].id.shown.max(need);
        }
    }
    // Duplicates take the longest of their run.
    for pair in order.windows(2) {
        if listed[pair[0]].id.same(&listed[pair[1]].id) {
            let shown = listed[pair[0]].id.shown.max(listed[pair[1]].id.shown);
            listed[pair[0]].id.shown = shown;
            listed[pair[1]].id.shown = shown;
        }
    }
}

/// A fix as data: `start..end` of the file replaced by `insert`, and why it is the fix (a help's
/// kind and its arguments, worded where it is printed).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Change<'f> {
    pub start: u32,
    pub end: u32,
    pub insert: &'f str,
    pub reason: HelpKind,
    pub args: [&'f str; 2],
    pub given: u8,
}

impl Change<'_> {
    pub(crate) fn args(&self) -> &[&str] {
        &self.args[..usize::from(self.given)]
    }
}

/// One finding of a command, placed in today's text: what every view prints and `--fix` reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Listed<'f> {
    /// The file it is drawn in; `None` for a finding of no file, drawn at [`Listed::about`].
    pub path: Option<&'f str>,
    pub about: Name,
    /// The file's text as this command read it, by its digest ([`text_id`]; of no bytes for a
    /// finding of no file). Never the text: a finding costs the command's frame this struct, and
    /// a file is read again, into scratch, where it is drawn or fixed (print.rs `Scratch`).
    pub read: blaze_ids::ContentId,
    pub area: Area,
    pub rule: u16,
    /// Its words' pieces, between 0x1F bytes.
    pub words: &'f [u8],
    /// Its item's start in the file.
    pub item: u32,
    /// File-relative.
    pub start: u32,
    pub end: u32,
    pub id: Id,
    pub fix: Option<Change<'f>>,
}

impl Listed<'_> {
    /// Its rule's name (`syntax`, `E0425`): the code a selector names and a view prints.
    pub(crate) fn code(&self) -> &'static str {
        crate::print::code_of(self.area, self.rule)
    }
}

/// The span of the "item" a finding at `at` of `text` is keyed by: the innermost item of the
/// outline holding it (`edition` given: a Rust file), else its line; and the item's chain.
/// The grammar a Rust file's outline is read in for its findings' ids, the same for every file:
/// the id is then a function of the text alone, never of whether this command has the file's
/// edition at hand (a certified repeat reads no file's row). Editions differ in a few keywords,
/// not in where items begin and end, so the items found are the file's own in all but contrived
/// text, and even there the id stays deterministic.
pub(crate) const ITEM_GRAMMAR: blaze_surface::Edition = blaze_surface::Edition::E2024;

pub(crate) fn item_of(text: &str, at: u32, edition: Option<blaze_surface::Edition>, frame: &Frame) -> (u32, u32, u64) {
    let erased = blaze_own::Erased::new(frame);
    let outline = edition.map(|edition| blaze_file_items::outline(text, edition, erased.handle()));
    item_in(outline.as_ref(), text, at)
}

/// The digest a [`Listed`] holds of its file's text: what `--fix` and the drawing compare the
/// file as it is now against.
pub(crate) fn text_id(text: &[u8]) -> blaze_ids::ContentId {
    blaze_ids::ContentId::of("blaze-cli a finding's file text", text)
}

/// [`item_of`] over an outline its caller made (one a file, whatever its findings' count):
/// `None` for a file that is not Rust.
pub(crate) fn item_in(outline: Option<&blaze_file_items::Outline<'_>>, text: &str, at: u32) -> (u32, u32, u64) {
    if let Some(outline) = outline {
        if let Some(item) = crate::work::item_holding(outline, at) {
            return (item.range.start, item.range.end, crate::work::item_chain(outline, item, text));
        }
    }
    let bytes = text.as_bytes();
    let at = (at as usize).min(bytes.len());
    let start = bytes[..at].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let end = bytes[at..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| at + i + 1);
    (start as u32, end as u32, 0)
}

/// Why `--fix` wrote nothing: each names what it is about (rule 19).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Refusal<'s> {
    /// No finding of this command has this id, code or path (an id printed before its text
    /// changed is no longer any finding's).
    NoMatch { selector: &'s str },
    /// The prefix begins more than one finding's id: the first few of them, and how many.
    Ambiguous { selector: &'s str, ids: [Option<Id>; 4], count: usize },
    /// The finding has no fix to apply.
    NoFix { id: Id, code: &'static str },
    /// Two fixes change overlapping bytes of one file.
    Overlap { a: Id, b: Id },
    /// The file changed between this command's check and its write.
    Changed { id: Id },
}

impl core::fmt::Display for Refusal<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Refusal::NoMatch { selector } => write!(f, "--fix {selector}: no finding of this check has this id, code or path (an id changes when its item's text does: check again for today's)"),
            Refusal::Ambiguous { selector, ids, count } => {
                write!(f, "--fix {selector}: the prefix begins {count} ids:")?;
                for id in ids.iter().flatten() {
                    write!(f, " {id}")?;
                }
                if count > ids.len() {
                    write!(f, " ..")?;
                }
                write!(f, "; give more of one")
            }
            Refusal::NoFix { id, code } => write!(f, "--fix {id}: this {code} finding has no fix to apply"),
            Refusal::Overlap { a, b } => write!(f, "--fix: the fixes of {a} and {b} change the same bytes: apply one, then check again"),
            Refusal::Changed { id } => write!(f, "--fix {id}: its file changed while this command ran: nothing written"),
        }
    }
}

/// How a fixed file is written: a strategy at one seam (rule 78).
pub(crate) trait Writer {
    fn replace(&self, cx: &Cx, root: &Dir<ReadWrite>, path: &Path, bytes: &[u8]) -> Result<(), CliError>;
}

/// The file replaced whole and durably: bstd's synced publish (a new file flushed, renamed over
/// the old, the directory synced). A crash leaves the old file or the new one.
#[derive(Default)]
pub(crate) struct Atomic;

impl Writer for Atomic {
    fn replace(&self, cx: &Cx, root: &Dir<ReadWrite>, path: &Path, bytes: &[u8]) -> Result<(), CliError> {
        root.publish(cx, path, Content::bytes(bytes), Durable::Yes)?;
        Ok(())
    }
}

/// THE PLANTED BUG of the crash test: the file truncated and written in place, never synced.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct InPlace;

#[cfg(test)]
impl Writer for InPlace {
    fn replace(&self, cx: &Cx, root: &Dir<ReadWrite>, path: &Path, bytes: &[u8]) -> Result<(), CliError> {
        let file = root.create(cx, path)?;
        file.append(cx, bytes, Durable::No)?;
        file.close(cx, Durable::No)?;
        Ok(())
    }
}

/// What `--fix` applied: the findings fixed, in file order.
pub(crate) struct Applied<'f> {
    pub fixed: Vec<Id, &'f Frame>,
    pub files: u32,
}

/// The findings `selectors` choose: indices into `listed`, each with a fix, or the refusal.
fn choose<'s, 'a>(listed: &[Listed<'_>], selectors: &[&'s str], frame: &'a Frame) -> Result<Vec<usize, &'a Frame>, Refusal<'s>> {
    let mut chosen: Vec<usize, &'a Frame> = Vec::new_in(frame);
    for &selector in selectors {
        let by_code = listed.iter().any(|l| l.code() == selector);
        let by_path = listed.iter().any(|l| l.path == Some(selector));
        let before = chosen.len();
        if selector == "all" || by_code || by_path {
            let mut first_without = None;
            for (at, l) in listed.iter().enumerate() {
                let hit = selector == "all" || l.code() == selector || l.path == Some(selector);
                match (hit, l.fix.is_some()) {
                    (true, true) => chosen.push(at),
                    (true, false) => first_without = first_without.or(Some(at)),
                    _ => {}
                }
            }
            if chosen.len() == before {
                return Err(match first_without {
                    Some(at) => Refusal::NoFix { id: listed[at].id, code: listed[at].code() },
                    None => Refusal::NoMatch { selector },
                });
            }
            continue;
        }
        // An id, or a prefix of one: every finding whose id it begins must be one finding.
        let mut ids: [Option<Id>; 4] = [None; 4];
        let mut count = 0usize;
        let mut first = None;
        for (at, l) in listed.iter().enumerate() {
            if !l.id.starts(selector) || ids.iter().flatten().any(|id| id.same(&l.id)) {
                continue;
            }
            if count < ids.len() {
                ids[count] = Some(l.id);
            }
            count += 1;
            first = first.or(Some(at));
        }
        let Some(first) = first else { return Err(Refusal::NoMatch { selector }) };
        if count > 1 {
            return Err(Refusal::Ambiguous { selector, ids, count });
        }
        if listed[first].fix.is_none() {
            return Err(Refusal::NoFix { id: listed[first].id, code: listed[first].code() });
        }
        chosen.push(first);
    }
    // One finding once (a selector may name it twice; one printed per configuration is one).
    chosen.sort_unstable_by(|&a, &b| (listed[a].path, listed[a].start, listed[a].id.full).cmp(&(listed[b].path, listed[b].start, listed[b].id.full)));
    chosen.dedup_by(|a, b| listed[*a].id.same(&listed[*b].id));
    Ok(chosen)
}

/// `blaze check --fix <selector>...` on the findings this command's check listed: every refusal
/// decided first, then each file replaced through `W`. `Ok(Err(..))` wrote nothing.
pub(crate) fn apply<'f, 's, W: Writer>(cx: &Cx, root: &Dir<ReadWrite>, writer: &W, listed: &[Listed<'f>], selectors: &[&'s str], frame: &'f Frame) -> Result<Result<Applied<'f>, Refusal<'s>>, CliError> {
    let mut chosen = match choose(listed, selectors, frame) {
        Ok(chosen) => chosen,
        Err(refusal) => return Ok(Err(refusal)),
    };
    // By file, then by the fix's range: two that overlap are neighbours.
    chosen.sort_unstable_by_key(|&at| (listed[at].path, listed[at].fix.map(|c| (c.start, c.end))));
    for pair in chosen.windows(2) {
        let (a, b) = (&listed[pair[0]], &listed[pair[1]]);
        let (Some(x), Some(y)) = (a.fix, b.fix) else { continue };
        // Two insertions at one point overlap too: their order would be a guess.
        if a.path == b.path && (y.start < x.end || (x.start == y.start && x.end == y.end)) {
            return Ok(Err(Refusal::Overlap { a: a.id, b: b.id }));
        }
    }
    // Each file as it is now must be the text the ids were made from: read once, here, and
    // kept for the write below (a finding holds its file's digest, never its text).
    let mut texts: Vec<&[u8], &Frame> = Vec::new_in(frame);
    let mut start = 0;
    while start < chosen.len() {
        let path = listed[chosen[start]].path;
        let end = start + chosen[start..].iter().take_while(|&&at| listed[at].path == path).count();
        let Some(path) = path else { return Ok(Err(Refusal::NoFix { id: listed[chosen[start]].id, code: listed[chosen[start]].code() })) };
        let mut now = Bytes::new_in(frame, "a file to fix");
        root.read_all(cx, &Path::new(path)?, &mut now)?;
        if text_id(now.as_slice()) != listed[chosen[start]].read {
            return Ok(Err(Refusal::Changed { id: listed[chosen[start]].id }));
        }
        texts.push(frame.alloc_slice_copy(now.as_slice()));
        start = end;
    }
    let mut fixed = Vec::with_capacity_in(chosen.len(), frame);
    let mut files = 0;
    let mut start = 0;
    while start < chosen.len() {
        let first = &listed[chosen[start]];
        let end = start + chosen[start..].iter().take_while(|&&at| listed[at].path == first.path).count();
        let text = texts[files as usize];
        let mut bytes: Vec<u8, &Frame> = Vec::with_capacity_in(text.len(), frame);
        let mut from = 0usize;
        for &at in chosen[start..end].iter() {
            let Some(change) = listed[at].fix else { continue };
            // A fix's range is in the text its digest names, which `text` is.
            let (Some(kept), true) = (text.get(from..change.start as usize), change.end as usize <= text.len()) else {
                return Err(CliError::Text { what: "a fix's range is outside its file" });
            };
            bytes.extend_from_slice(kept);
            bytes.extend_from_slice(change.insert.as_bytes());
            from = change.end as usize;
            fixed.push(listed[at].id);
        }
        bytes.extend_from_slice(&text[from..]);
        writer.replace(cx, root, &Path::new(first.path.unwrap_or(""))?, &bytes)?;
        files += 1;
        start = end;
    }
    Ok(Ok(Applied { fixed, files }))
}

/// THE CHECKER'S FIX, ready for the day types are wired (typeck.md, "Fixes ranked by what is
/// left"): one body's `Checked::suggests`, its item's text and place, made into findings with
/// their ids and fixes. Each hole is an E0425 at a one-segment value name; `suggest::pick`
/// chooses from the record and the item's text; the fix's two item-relative ranges become one
/// file edit (the hole replaced by the item's own bytes of the chosen name). `type_words` prints
/// a candidate's type from the record's type table (the checker's printer). Called by nothing
/// yet: see main.rs `request`, where the TYPES stage's records are drawn.
#[expect(clippy::too_many_arguments, reason = "a body's record and the item's place in today's text, each one fact")]
pub(crate) fn checker_findings<'f>(
    suggests: &blaze_typeck::suggest::Suggests<'_>,
    path: &'f str,
    text: &'f [u8],
    (item, item_end, chain): (u32, u32, u64),
    about: Name,
    type_words: &dyn Fn(blaze_typeck::PTy) -> &'f str,
    alloc: Alloc<'_>,
    into: &mut Vec<Listed<'f>, &'f Frame>,
) -> Result<(), blaze_typeck::suggest::PickError> {
    use blaze_typeck::suggest::{Pick, Rules, pick};
    let item_text = text.get(item as usize..item_end as usize).unwrap_or(&[]);
    let words_of = |r: blaze_typeck::Range| -> &'f str {
        let bytes = text.get((item + r.start) as usize..(item + r.end) as usize).unwrap_or(&[]);
        core::str::from_utf8(bytes).unwrap_or("")
    };
    /// The item's text as `pick` reads names from it.
    struct ItemText<'t>(&'t [u8]);
    impl blaze_typeck::suggest::NameText for ItemText<'_> {
        fn bytes(&self, r: blaze_typeck::Range) -> Option<&[u8]> {
            self.0.get(r.start as usize..r.end as usize)
        }
    }
    let names = ItemText(item_text);
    for hole in suggests.holes.iter() {
        let picked = pick(suggests, hole, &names, Rules::All, alloc)?;
        // `pick` trimmed the candidate to its name: `fix.from` is the bytes it inserts.
        let why = match picked {
            Pick::Unused(c) => Some((HelpKind::NeverUsed, Some(c.ty))),
            Pick::Spelled(_) => Some((HelpKind::SpelledAlike, None)),
            Pick::Nothing | Pick::Tied(..) => None,
        };
        let fix = picked.fix(hole).zip(why).map(|(fix, (reason, ty))| {
            let name = words_of(fix.from);
            let (args, given) = match ty {
                Some(ty) => ([name, type_words(ty)], 2),
                None => ([name, ""], 1),
            };
            Change { start: item + fix.at.start, end: item + fix.at.end, insert: name, reason, args, given }
        });
        let written = &text[(item + hole.at.start) as usize..(item + hole.at.end) as usize];
        let code = "E0425";
        into.push(Listed {
            path: Some(path),
            about,
            read: text_id(text),
            area: Area::Types,
            rule: 425,
            words: written,
            item,
            start: item + hole.at.start,
            end: item + hole.at.end,
            id: Id::of(path.as_bytes(), chain, item_text, code, (hole.at.start, hole.at.end)),
            fix,
        });
    }
    Ok(())
}
