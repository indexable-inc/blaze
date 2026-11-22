//! Stores of other forms beside this blaze's (D160: a store's format is part of its place).
//!
//! The one store of this blaze is `<home>/.blaze/`[`STORE_DIR`] (`v2/store/f5s3`). Beside it,
//! under [`STORE_ROOT`], a home may hold stores this blaze does not read:
//!
//! - THE LEGACY LAYOUT: a store of format 4 or before, whose files (`head`, `flip`, `seg-*`,
//!   `manifests-*`: blaze-cas `store_leaf`) lay directly in `v2/store`;
//! - A FOLDER OF ANOTHER FORM: `v2/store/f<N>s<M>` with forms other than this blaze's (blaze-cas
//!   `PlaceName::parse`): an older blaze's, or a newer one's (two binaries share a home).
//!
//! Each is [`OtherForm`]: never opened, never read, never migrated. [`note_once`] says each by
//! name with its size, once in the life of this blaze's store, recorded as a root of it
//! ([`OTHER_FORMS_LOOKED`], committed with the command's records after the answer: the
//! mechanism of store.rs `old_home`). Removing them is cleanup's (clean.rs): `blaze clean`
//! removes every one, and the sweep after the answer removes one unused for
//! [`EXPIRY_DAYS`] ([`expired`]). A name under `v2/store` of no store's shape (`notes`, a
//! `f05s2`) is none of blaze's here: not said, not removed.
//!
//! A store of THIS form that does not read stays the store's typed refusal (blaze-cas): it is
//! never taken for empty, never rebuilt quietly, and never listed here.

use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, EntryKind, FsError, ReadWrite};
use bstd::out::Out;
use bstd::path::Path;

use blaze_cas::plain::{CACHEDIR_SIGNATURE, CACHEDIR_TAG, FORMS, Lent, PlaceName, store_leaf};
use blaze_core::Driver;

use super::{STORE_DIR, STORE_ROOT, StoreAlloc};
use crate::CliError;
use crate::store::StoreFailure;

/// The root that says the other forms were looked for, in this blaze's store: absent in a
/// store just made, so the look runs once in its life.
pub(crate) const OTHER_FORMS_LOOKED: &str = "blaze.other-forms-looked";

/// How long a store of another form is kept after its last use: the roster's window
/// (roster.rs `WINDOW_DAYS`, 30), the time a binary that ran is taken as live. A blaze of that
/// form that still runs uses it, and so keeps it.
pub(crate) const EXPIRY_DAYS: u64 = crate::roster::WINDOW_DAYS as u64;

const DAY_NS: u64 = 86_400 * 1_000_000_000;

/// The most stores of other forms one look lists (rule 18): one a form a blaze on this machine
/// ever wrote. Past it, the look refuses naming the count.
pub(crate) const OTHER_FORMS_MOST: usize = 16;

/// Which store of another form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FormKind {
    /// Format 4 or before: its files directly in `v2/store`.
    Legacy,
    /// `v2/store/<name>`, of the forms `name` says.
    Folder(PlaceName),
}

/// One store of another form, as one listing found it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OtherForm {
    pub kind: FormKind,
    /// Its files and their lengths together.
    pub files: u64,
    pub bytes: u64,
    /// Its last use: the latest change time of its head and of its folder (the head is
    /// rewritten by every flip of a command that used it), as the file system keeps it.
    pub used_ns: u64,
}

/// What [`list`] found: at most [`OTHER_FORMS_MOST`], in the names' order (the legacy layout
/// first).
#[derive(Clone, Copy, Debug)]
pub(crate) struct OtherForms {
    found: [Option<OtherForm>; OTHER_FORMS_MOST],
    len: usize,
}

impl OtherForms {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &OtherForm> {
        self.found[..self.len].iter().flatten()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// `forms` and `one` more (clean.rs's planted sweep that takes its own folder).
    pub(crate) fn with(mut forms: OtherForms, one: OtherForm) -> Result<OtherForms, CliError> {
        forms.push(one)?;
        Ok(forms)
    }

    fn push(&mut self, form: OtherForm) -> Result<(), CliError> {
        if self.len == OTHER_FORMS_MOST {
            return Err(CliError::Store(StoreFailure::OtherForms { most: OTHER_FORMS_MOST }));
        }
        self.found[self.len] = Some(form);
        self.len += 1;
        Ok(())
    }
}

/// The store root's path beneath blaze's folder in the home.
pub(crate) fn root() -> Result<Path, CliError> {
    Ok(Path::new(STORE_ROOT)?)
}

fn look(error: FsError) -> CliError {
    CliError::Store(StoreFailure::Look { folder: STORE_ROOT, error })
}

/// Every store of another form in the store root `root` beneath `blaze`: [`STORE_ROOT`] beneath
/// the home's `.blaze` (store.rs), or `.blaze/v2/store` beneath the home (clean.rs).
pub(crate) fn list(cx: &Cx, blaze: &Dir<ReadWrite>, root: &Path) -> Result<OtherForms, CliError> {
    let mut out = OtherForms { found: [None; OTHER_FORMS_MOST], len: 0 };
    let Some(entries) = blaze.entries(cx, root).map_err(look)? else { return Ok(out) };
    let mut legacy = OtherForm { kind: FormKind::Legacy, files: 0, bytes: 0, used_ns: 0 };
    for i in 0..entries.len() {
        if entries.kind(i) == EntryKind::File && store_leaf(entries.name(i)) {
            legacy.files += 1;
            legacy.bytes += entries.len_of(i);
        }
    }
    if legacy.files > 0 {
        legacy.used_ns = used_ns(cx, blaze, root)?;
        out.push(legacy)?;
    }
    for i in 0..entries.len() {
        if entries.kind(i) != EntryKind::Dir {
            continue;
        }
        let Some(forms) = PlaceName::parse(entries.name(i)) else { continue };
        if forms == FORMS {
            continue;
        }
        let path = entries.path(i)?;
        let mut form = OtherForm { kind: FormKind::Folder(PlaceName::of(forms)), files: 0, bytes: 0, used_ns: 0 };
        if let Some(inside) = blaze.entries(cx, &path).map_err(look)? {
            for j in 0..inside.len() {
                if matches!(inside.kind(j), EntryKind::File | EntryKind::Link) {
                    form.files += 1;
                    form.bytes += inside.len_of(j);
                }
            }
        }
        form.used_ns = used_ns(cx, blaze, &path)?;
        out.push(form)?;
    }
    Ok(out)
}

/// The latest change of the folder `at` and of the head in it.
///
/// WHY A MODIFICATION TIME ALONE, where bstd's `Stamp` says it is compared whole, for equality
/// only (fs.rs, `Stamp`). That rule guards a PROOF: an equal stamp is taken to say "these
/// bytes did not change", which a racy write within the file system's clock step could make
/// false, so nothing may read one field of it, or order by it, to skip work. This is no proof
/// and skips nothing: it is a coarse age, read against a 30-day period, of data that is
/// derived. Nothing better is known of an other form's use: this blaze cannot read that
/// store's head or roots (they are of a form it does not read, which is why it is there), and
/// the roster (roster.rs) records binaries, not the forms they write. Each way it can be wrong
/// is safe: a clock step of seconds is nothing against 30 days; a time in the future (a clock
/// set back) keeps the store ([`expired`]); a store an old blaze still uses is rewritten (its
/// head) on each of that blaze's commits, so it stays younger than the period while that blaze
/// runs; and one removed too early costs that blaze a rebuild of its own records, never a
/// wrong answer.
fn used_ns(cx: &Cx, blaze: &Dir<ReadWrite>, at: &Path) -> Result<u64, CliError> {
    let folder = blaze.stat(cx, at).map_err(look)?.map_or(0, |it| it.stamp.modified_ns);
    let head = blaze.stat(cx, &at.join(&Path::new("head")?)).map_err(look)?.map_or(0, |it| it.stamp.modified_ns);
    Ok(folder.max(head))
}

/// THE RULE OF EXPIRY: a store of another form is removed by the sweep once `now_ns` is past
/// its last use by more than [`EXPIRY_DAYS`]. A last use in the future (a clock set back) is
/// not past: kept.
pub(crate) fn expired(form: &OtherForm, now_ns: u64) -> bool {
    now_ns.saturating_sub(form.used_ns) > EXPIRY_DAYS * DAY_NS
}

/// The line said of `form` (no newline).
pub(crate) struct Note<'a>(pub &'a OtherForm);

impl core::fmt::Display for Note<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let OtherForm { kind, files, bytes, .. } = self.0;
        let noun = if *files == 1 { "file" } else { "files" };
        let size = blaze_report::ui::fmt_bytes(*bytes);
        match kind {
            FormKind::Legacy => write!(f, "blaze: note: ~/.blaze/{STORE_ROOT} (format <=4 · {files} {noun} · {size})")?,
            FormKind::Folder(name) => write!(f, "blaze: note: ~/.blaze/{STORE_ROOT}/{} (other version · {files} {noun} · {size})", name.as_str())?,
        }
        write!(
            f,
            " — active: ~/.blaze/{STORE_DIR}; run `blaze clean` to remove (auto-expires after {EXPIRY_DAYS} days)"
        )
    }
}

/// THE NOTE, once in the life of this blaze's store: each store of another form by name and
/// size on `err` (a note, not an error: the command goes on), then the root that says it was
/// said, committed after the answer with the command's records. The store root's cache
/// directory tag is written then too when it is missing (a home whose first store is of this
/// form: the tag stays at `v2/store`, clean.rs checks it there). The stores found, said.
pub(crate) fn note_once<D: Driver>(cx: &Cx, blaze: &Dir<ReadWrite>, lent: &Lent<D, StoreAlloc>, err: &Out) -> Result<u32, CliError> {
    if lent.roots().iter().any(|(name, _)| name == OTHER_FORMS_LOOKED) {
        return Ok(0);
    }
    let tag = root()?.join(&Path::new(CACHEDIR_TAG)?);
    if blaze.stat(cx, &tag).map_err(look)?.is_none() {
        blaze.publish(cx, &tag, Content::bytes(CACHEDIR_SIGNATURE), Durable::No).map_err(look)?;
    }
    let found = list(cx, blaze, &root()?)?;
    if found.len() > 0 {
        let total_bytes: u64 = found.iter().map(|it| it.bytes).sum();
        let noun = if found.len() == 1 { "store" } else { "stores" };
        let headline = std::format!(
            "{} other-version {noun} ({}) — active: ~/.blaze/{STORE_DIR}",
            found.len(),
            blaze_report::ui::fmt_bytes(total_bytes),
        );
        let owned_rows: std::vec::Vec<(std::string::String, std::string::String)> = found
            .iter()
            .map(|form| {
                let path = match &form.kind {
                    FormKind::Legacy => std::format!("~/.blaze/{STORE_ROOT}"),
                    FormKind::Folder(name) => std::format!("~/.blaze/{STORE_ROOT}/{}", name.as_str()),
                };
                let label = match &form.kind {
                    FormKind::Legacy => "format <=4",
                    FormKind::Folder(_) => "other version",
                };
                let fnoun = if form.files == 1 { "file" } else { "files" };
                let detail = std::format!(
                    "{label} · {} {fnoun} · {}",
                    blaze_report::ui::fmt_count(form.files),
                    blaze_report::ui::fmt_bytes(form.bytes)
                );
                (path, detail)
            })
            .collect();
        let rows: std::vec::Vec<(&str, &str)> = owned_rows.iter().map(|(l, r)| (l.as_str(), r.as_str())).collect();
        let action = std::format!("run `blaze clean` to remove (auto-expires after {EXPIRY_DAYS} days)");
        let rendered = blaze_report::ui::Theme::PLAIN.notice_group(
            "note",
            blaze_report::palette::Token::Warning,
            &headline,
            &rows,
            Some(&action),
        );
        for line in rendered.lines() {
            err.line(cx, format_args!("{line}"))?;
        }
    }
    lent.root(OTHER_FORMS_LOOKED, &[]).map_err(|error| CliError::Store(StoreFailure::Place(super::PlaceError::Lent(error))))?;
    Ok(found.len() as u32)
}

#[cfg(test)]
#[path = "forms_tests.rs"]
mod tests;
