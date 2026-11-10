//! `@` file mentions in a chat's composer (scene/widgets/chat.flecs `hn.ChatMention`), Claude Code's way.
//!
//! An `@` that starts a word (the draft's start or after a blank), the caret right after what follows it with no
//! selection and no marked text, opens a menu above the composer of the places it could name: files and folders under
//! the chat's directory, best match first (a match in a name beats one across folders, a name that starts with what is
//! typed beats one that holds it, a shorter path beats a longer). With nothing typed after the `@` it shows the
//! directory's own entries, folders first. What starts with `/`, `~/` or `~` names a folder by itself: its entries whose
//! names start with the rest (dotfiles only once a `.` is typed).
//!
//! Up and down move the highlight (the pointer moves it too), Tab or Return takes it: a file becomes one atom in the
//! draft (chat.rs `Atom`: a chip of its icon and name, moved over, selected and erased whole, sent as `@<path>`) and a
//! space, so the next word starts; a folder becomes `@<path>/` in words and keeps the menu open on its entries.
//! Return with nothing matched sends the draft as usual. Escape closes the menu until the caret leaves that `@` word.
//!
//! Beside the list, a preview of the highlighted place (`preview`, a fixed box so the menu keeps its size as the highlight
//! moves): its path, then a PNG or JPEG drawn whole in the box, a text file's first lines in the code face coloured by
//! its language, or a folder's entries the Finder's way (chat/peek.rs's reader, the hover cards'); what cannot be read
//! says why. Each place is read once while the menu is open.
//!
//! The places come from an index of the directory built once on a thread of its own and kept [`FRESH`] (asked again
//! after that when a menu opens on it): `git ls-files --cached --others --exclude-standard` in a git work tree (what
//! git ignores is left out), and in a directory outside one, a walk that skips dot-folders, `node_modules` and `target`,
//! at most [`WALK_MAX`] entries. A git that fails for any other reason says why in the menu, naming the directory.
//! While the index builds the menu says so, and the app is woken when it lands.
use super::*;
use harness_native_ui_inline::files;
use harness_native_ui_view::menu::{self as vmenu, Nav};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The most rows the menu shows.
const ROWS: usize = 8;
/// How long a directory's index is used before a menu opening on it asks for it again.
const FRESH: Duration = Duration::from_secs(30);
/// The most entries a walk (a directory outside git) or an index keeps.
const WALK_MAX: usize = 50_000;
const INDEX_MAX: usize = 400_000;
/// Folders a walk never goes into.
const SKIP: &[&str] = &["node_modules", "target"];

/// A place the menu can offer: its path as it goes after the `@` (relative to the chat's directory, or as typed for an
/// absolute or `~` one), a folder's with a trailing `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Place {
    pub path: String,
    pub dir: bool,
}

/// One directory's index: its places, when it was built, the error that stopped it.
struct Listing {
    places: Arc<Vec<Place>>,
    at: Instant,
    error: Option<String>,
    rev: u64,
}

/// Every directory's index, and which are being built.
#[derive(Default)]
struct Indexes {
    by_dir: HashMap<String, Listing>,
    building: BTreeSet<String>,
    rev: u64,
}

fn indexes() -> &'static Mutex<Indexes> {
    static INDEXES: OnceLock<Mutex<Indexes>> = OnceLock::new();
    INDEXES.get_or_init(Mutex::default)
}

/// Directory `dir`'s index as it is now (None while the first is built), asking for a new one on a thread when there is
/// none or it is older than [`FRESH`]; `wake` is called once it lands.
fn index_of(dir: &str, wake: &crate::app::Wake) -> Option<(Arc<Vec<Place>>, Option<String>, u64)> {
    let mut ix = indexes().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stale = ix.by_dir.get(dir).is_none_or(|l| l.at.elapsed() > FRESH);
    if stale && ix.building.insert(dir.to_owned()) {
        let (dir_owned, wake) = (dir.to_owned(), wake.clone());
        let spawned = std::thread::Builder::new().name("mention-index".into()).spawn(move || {
            let (places, error) = match list(&dir_owned) {
                Ok(p) => (p, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            let mut ix = indexes().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ix.rev += 1;
            let rev = ix.rev;
            ix.building.remove(&dir_owned);
            ix.by_dir.insert(dir_owned, Listing { places: Arc::new(places), at: Instant::now(), error, rev });
            drop(ix);
            wake();
        });
        if let Err(e) = spawned {
            ix.building.remove(dir);
            eprintln!("harness-native: mention: indexing {dir}: no thread: {e}");
        }
    }
    ix.by_dir.get(dir).map(|l| (l.places.clone(), l.error.clone(), l.rev))
}

/// Every place under `dir`: git's files (tracked and untracked, less what it ignores) and the folders that hold them,
/// or, outside a git work tree, a walk.
fn list(dir: &str) -> Result<Vec<Place>, String> {
    let out = std::process::Command::new("git")
        .args(["-C", dir, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git ls-files in {dir}: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        if why.contains("not a git repository") {
            return Ok(walk(dir));
        }
        return Err(format!("git ls-files in {dir}: {}", why.trim()));
    }
    Ok(from_git(&String::from_utf8_lossy(&out.stdout)))
}

/// git's NUL-separated file list as places: each file, and once each folder above one.
fn from_git(z: &str) -> Vec<Place> {
    let mut dirs: BTreeSet<&str> = BTreeSet::new();
    let mut places: Vec<Place> = Vec::new(); // alloc: one directory's index, kept FRESH
    for f in z.split('\0').filter(|f| !f.is_empty()).take(INDEX_MAX) {
        let mut at = 0;
        while let Some(i) = f[at..].find('/') {
            dirs.insert(&f[..at + i + 1]);
            at += i + 1;
        }
        places.push(Place { path: f.to_owned(), dir: false });
    }
    places.extend(dirs.into_iter().map(|d| Place { path: d.to_owned(), dir: true }));
    places
}

/// A directory outside git, breadth first: its entries and its folders', less dot-folders and [`SKIP`], at most
/// [`WALK_MAX`].
fn walk(dir: &str) -> Vec<Place> {
    let mut places: Vec<Place> = Vec::new(); // alloc: one directory's index, kept FRESH
    let mut queue = std::collections::VecDeque::from([String::new()]);
    while let Some(rel) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(std::path::Path::new(dir).join(&rel)) else { continue };
        for e in read.filter_map(Result::ok) {
            if places.len() >= WALK_MAX {
                return places;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && (name.starts_with('.') || SKIP.contains(&name.as_str())) {
                continue;
            }
            let path = format!("{rel}{name}{}", if is_dir { "/" } else { "" });
            if is_dir {
                queue.push_back(path.clone());
            }
            places.push(Place { path, dir: is_dir });
        }
    }
    places
}

/// The `@` word the caret ends in draft `d`: where its `@` is and what follows it (None when there is none, or the
/// caret is not at its end, or there is a selection or marked text).
pub(super) fn token(d: &Draft) -> Option<(usize, &str)> {
    if d.caret != d.anchor || d.marked.is_some() {
        return None;
    }
    if !d.text[d.caret..].chars().next().is_none_or(char::is_whitespace) {
        return None;
    }
    let before = &d.text[..d.caret];
    let start = before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    Some((start, before[start..].strip_prefix('@')?))
}

/// How well `q` names `path` (higher is better), or None: every character of `q` in order, case-insensitively. A match
/// inside the last name counts most (more at its start, most when it starts it), then matches at the start of a word,
/// then runs; a deeper or longer path a little less.
fn score(q: &str, path: &str) -> Option<i64> {
    let trimmed = path.trim_end_matches('/');
    let name_at = trimmed.rfind('/').map_or(0, |i| i + 1);
    let name = &trimmed[name_at..];
    let lower = |s: &str| s.to_lowercase();
    let (ql, nl) = (lower(q), lower(name));
    let pl = lower(trimmed);
    let mut s = search_in(&ql, &pl)?;
    // What is typed, whole, in the name counts most; scattered through the name a little; whole across the path some.
    if nl.contains(&ql) {
        s += 100;
        if nl.starts_with(&ql) {
            s += 60;
        }
        if nl == ql || nl.split('.').next() == Some(ql.as_str()) {
            s += 60;
        }
    } else if let Some(n) = search_in(&ql, &nl) {
        s += 10 + n;
        // Strewn through a name that starts as what is typed does.
        if ql.chars().next().is_some_and(|c| nl.starts_with(c)) {
            s += 40;
        }
    } else if pl.contains(&ql) {
        s += 30;
    }
    if !path.ends_with('/') {
        s += 1;
    }
    let depth = trimmed.matches('/').count() as i64;
    Some(s * 16 - depth * 8 - trimmed.len() as i64)
}

/// `q` (lowercase) as a subsequence of `text` (lowercase): a point a character, more at a word's start and in a run.
fn search_in(q: &str, text: &str) -> Option<i64> {
    let chars: Vec<char> = text.chars().collect(); // alloc: one path, while a query is typed
    let (mut at, mut s, mut prev) = (0usize, 0i64, None::<usize>);
    for c in q.chars() {
        let i = (at..chars.len()).find(|&i| chars[i] == c)?;
        let start = i == 0 || matches!(chars[i - 1], '/' | '_' | '-' | '.' | ' ');
        s += 1 + if start { 6 } else { 0 } + if prev == Some(i.wrapping_sub(1)) { 4 } else { 0 };
        // A gap since the last match costs a little (at most 3), so a run beats the same letters strewn about.
        s -= prev.map_or(0, |p| (i - p - 1).min(3) as i64);
        prev = Some(i);
        at = i + 1;
    }
    Some(s)
}

/// The best [`ROWS`] of `places` for `q`: with nothing typed, the directory's own entries, folders first; else by
/// [`score`], ties to the shorter path, then by name.
pub(super) fn rank(places: &[Place], q: &str) -> Vec<Place> {
    if q.is_empty() {
        let mut top: Vec<&Place> = places.iter().filter(|p| !p.path.trim_end_matches('/').contains('/')).collect(); // alloc: the menu's rows
        top.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase())));
        return top.into_iter().take(ROWS).cloned().collect();
    }
    let mut hits: Vec<(i64, &Place)> = places.iter().filter_map(|p| Some((score(q, &p.path)?, p))).collect(); // alloc: the matches, once per query
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.len().cmp(&b.1.path.len())).then_with(|| a.1.path.cmp(&b.1.path)));
    hits.into_iter().take(ROWS).map(|(_, p)| p.clone()).collect()
}

/// What follows the `@` when it names a folder by itself (`/`, `~/`, `~`): that folder's entries whose names start
/// with the rest, folders first (dotfiles once the rest starts with `.`); `home` stands for `~`.
fn browse(q: &str, home: &str) -> Result<Vec<Place>, String> {
    let (folder, rest) = match q.rfind('/') {
        Some(i) => (&q[..=i], &q[i + 1..]),
        None => (q, ""),
    };
    let folder = if folder == "~" { "~/" } else { folder };
    let real = match folder.strip_prefix('~') {
        Some(r) => format!("{home}{r}"),
        None => folder.to_owned(),
    };
    let read = std::fs::read_dir(&real).map_err(|e| format!("{real}: {e}"))?;
    let rl = rest.to_lowercase();
    let mut out: Vec<Place> = read // alloc: the folder's matching entries, once per query
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name.starts_with('.') && !rest.starts_with('.')) || !name.to_lowercase().starts_with(&rl) {
                return None;
            }
            let dir = e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir()));
            Some(Place { path: format!("{folder}{name}{}", if dir { "/" } else { "" }), dir })
        })
        .collect();
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase())));
    out.truncate(ROWS);
    Ok(out)
}

/// A chat's mention menu: the `@` word it was last filled for (its directory, where its `@` is, what follows, the
/// index's revision), its rows, the note it shows instead, its highlight, and the `@` Escape closed.
#[derive(Default)]
pub(in crate::app) struct Mention {
    key: Option<(String, usize, String, u64)>,
    rows: Vec<Place>,
    note: Option<String>,
    nav: Nav,
    shut: Option<usize>,
    /// Each place previewed while the menu is open, by its path as offered, read once (cleared when it closes).
    previews: HashMap<String, Preview>,
}

/// What the preview shows of a place: a picture and its size in pixels, or what chat/peek.rs reads of it.
#[derive(Clone)]
enum Preview {
    Image { path: String, w: u32, h: u32 },
    Peeked(super::peek::Peeked),
}

/// The pixel size of the PNG or JPEG at `path` from its header, without decoding it (None for any other file).
pub(super) fn image_size(path: &str) -> Option<(u32, u32)> {
    use std::io::Read;
    let lower = path.to_lowercase();
    let png = lower.ends_with(".png");
    if !png && !lower.ends_with(".jpg") && !lower.ends_with(".jpeg") {
        return None;
    }
    let mut head = Vec::new(); // alloc: a picture's header, once a preview
    std::fs::File::open(path).ok()?.take(64 << 10).read_to_end(&mut head).ok()?;
    if png {
        let be = |i: usize| head.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        return (head.starts_with(b"\x89PNG")).then(|| Some((be(16)?, be(20)?))).flatten();
    }
    // A JPEG's size is in its first start-of-frame segment (SOF0..SOF15 less DHT, JPG and DAC).
    let mut i = 2;
    while i + 9 < head.len() && head[i] == 0xFF {
        let marker = head[i + 1];
        let len = usize::from(u16::from_be_bytes([head[i + 2], head[i + 3]]));
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let h = u32::from(u16::from_be_bytes([head[i + 5], head[i + 6]]));
            let w = u32::from(u16::from_be_bytes([head[i + 7], head[i + 8]]));
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Read what the preview shows of `path` (absolute): a picture's size, else a file's first `lines` lines with each cut
/// to `cols` characters (the box cuts the rest; code never wraps), else a folder's first `entries`.
fn read_preview(path: &str, lines: usize, cols: usize, entries: usize) -> Preview {
    if let Some((w, h)) = image_size(path).filter(|(w, h)| *w > 0 && *h > 0) {
        return Preview::Image { path: path.to_owned(), w, h };
    }
    match super::peek::peek_place(path, lines, entries) {
        super::peek::Peeked::Text(t) => {
            let cut: Vec<String> = t // alloc: the preview's lines, read once
                .lines()
                .map(|l| {
                    let l = l.replace('\t', "    ");
                    match l.char_indices().nth(cols) {
                        Some((k, _)) => l[..k].to_owned(),
                        None => l,
                    }
                })
                .collect();
            Preview::Peeked(super::peek::Peeked::Text(cut.join("\n")))
        }
        other => Preview::Peeked(other),
    }
}

impl Mention {
    /// Bring the menu up to draft `d` in directory `dir`: whether it is open.
    fn update(&mut self, d: &Draft, dir: &str, home: &str, wake: &crate::app::Wake) -> bool {
        let Some((start, q)) = token(d) else {
            (self.key, self.shut) = (None, None);
            self.previews.clear();
            return false;
        };
        if self.shut.is_some_and(|s| s != start) {
            self.shut = None;
        }
        if self.shut.is_some() {
            return false;
        }
        let fixed = q.starts_with('/') || q.starts_with('~');
        let (index, error, rev) = if fixed { (None, None, 0) } else {
            match index_of(dir, wake) {
                Some((p, e, r)) => (Some(p), e, r),
                None => (None, None, 0),
            }
        };
        let key = (dir.to_owned(), start, q.to_owned(), rev);
        if self.key.as_ref() == Some(&key) {
            return true;
        }
        let same_query = self.key.as_ref().is_some_and(|k| k.1 == start && k.2 == q);
        (self.rows, self.note) = if fixed {
            match browse(q, home) {
                Ok(r) if r.is_empty() => (r, Some("No matches".to_owned())),
                Ok(r) => (r, None),
                Err(e) => (Vec::new(), Some(e)),
            }
        } else {
            match (index, error) {
                (_, Some(e)) => (Vec::new(), Some(e)),
                (None, None) => (Vec::new(), Some(format!("Indexing {dir}\u{2026}"))),
                (Some(p), None) => {
                    let r = rank(&p, q);
                    let note = r.is_empty().then(|| if q.is_empty() { "Empty folder".to_owned() } else { "No matches".to_owned() });
                    (r, note)
                }
            }
        };
        if !same_query || self.nav.hot.is_none_or(|h| h >= self.rows.len()) {
            self.nav = Nav::default();
            if !self.rows.is_empty() {
                self.nav.hot = Some(0);
            }
        }
        self.key = Some(key);
        true
    }
}

impl App {
    /// Chat object `e`'s directory (the home folder when it has none).
    pub(super) fn chat_cwd(&self, e: u64) -> String {
        self.world
            .world_ref()
            .entity_from_id(e)
            .try_cloned::<&model::Cwd>()
            .map_or_else(|| std::env::var("HOME").unwrap_or_default(), |c| c.s.as_str().to_owned())
    }

    /// Chat pane `pv`'s mention menu (its composer card's `mention` slot) for its draft, while the chat has the keys.
    pub(super) fn fill_chat_mention(&self, pv: View<'_>, sid: u64, st: &mut ChatState, cwd: &str, focused: bool) {
        let w = self.world.world_ref();
        let Some(card) = composer(pv) else { return };
        let Some(slot) = card.part("mention") else { return };
        let home = std::env::var("HOME").unwrap_or_default();
        let open = focused && st.pick.is_none() && st.mention.update(&st.draft, cwd, &home, &self.link.waker());
        view::for_each_of(slot, open.then_some(("plate", "hn.ChatMention", ())), |_, (), _| {});
        let Some(plate) = slot.part("plate").filter(|_| open) else { return };
        plate.anchored(card.id());
        let Some(items) = plate.part("list").and_then(|l| l.part("items")) else { return };
        let m = &mut st.mention;
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        let items_id = items.id();
        let under = hovered
            .and_then(|h| std::iter::successors(Some(h), |n| n.parent()).take(16).find(|n| n.parent().is_some_and(|p| p.id() == items_id)))
            .and_then(|row| row.name().strip_prefix('r').and_then(|i| i.parse::<usize>().ok()))
            .filter(|&i| i < m.rows.len());
        m.nav.pointer(under);
        let hot = m.nav.hot;
        let rows = m.rows.iter().enumerate().map(|(i, p)| (format!("r{i}"), "hn.ChatMentionItem", Some((i, p))));
        let note = m.note.as_ref().map(|_| ("note".to_owned(), "ui.MenuNote", None));
        view::for_each_of(items, rows.chain(note), |v, r, _| match r {
            Some((i, p)) => {
                vmenu::light(v.entity(), hot == Some(i));
                v.action(&format!("chat:mention:{sid}:{i}"));
                let trimmed = p.path.trim_end_matches('/');
                let (folder, name) = match trimmed.rfind('/') {
                    Some(k) => (&p.path[..=k], &p.path[k + 1..]),
                    None => ("", p.path.as_str()),
                };
                if let Some(icon) = v.part("lead").and_then(|l| l.part("icon")) {
                    icon.drawing(files::drawing(files::kind_at(trimmed, p.dir)));
                }
                if let Some(l) = v.part("label") {
                    l.text(name);
                }
                if let Some(l) = v.part("about") {
                    l.text(folder.trim_end_matches('/'));
                }
            }
            None => {
                if let Some(l) = v.part("label") {
                    l.text(m.note.as_deref().unwrap_or_default());
                }
            }
        });
        if let Some(pane) = plate.part("preview") {
            let hot = hot.and_then(|i| m.rows.get(i)).cloned();
            self.fill_mention_preview(pane, m, hot.as_ref(), cwd, &home);
        }
    }

    /// The preview pane for place `p` (none: empty) offered from directory `cwd`: its path, then its picture, first
    /// lines or entries.
    fn fill_mention_preview(&self, pane: View<'_>, m: &mut Mention, p: Option<&Place>, cwd: &str, home: &str) {
        use super::peek::Peeked;
        let w = self.world.world_ref();
        let look = crate::look::mention_look(w);
        let real = p.map(|p| {
            let path = p.path.trim_end_matches('/');
            match path.strip_prefix('~') {
                Some(r) => format!("{home}{r}"),
                None if path.starts_with('/') => path.to_owned(),
                None => format!("{}/{path}", cwd.trim_end_matches('/')),
            }
        });
        let preview = match (p, real.as_deref()) {
            (Some(p), Some(real)) => {
                // As many as fit under the path: more would push the box's words up over it.
                let fit = |h: f32| (look.image_h / h.max(1.0)).floor().max(1.0) as usize;
                let (lines, cols, entries) = (fit(look.line), look.cols as usize, fit(look.entry));
                Some(m.previews.entry(p.path.clone()).or_insert_with(|| read_preview(real, lines, cols, entries)).clone())
            }
            _ => None,
        };
        let (box_w, box_h) = (look.image_w, look.image_h);
        let head = p.map(|_| ("head", "hn.ChatMentionHead", None));
        let body = preview.as_ref().map(|pv| match pv {
            Preview::Image { .. } => ("image", "hn.ChatMentionImage", Some(pv)),
            Preview::Peeked(Peeked::Text(_)) => ("code", "hn.ChatMentionCode", Some(pv)),
            Preview::Peeked(Peeked::Note(_)) => ("why", "hn.PeekMeta", Some(pv)),
            Preview::Peeked(Peeked::Folder { .. }) => ("entries", "hn.PeekList", Some(pv)),
        });
        let more = match &preview {
            Some(Preview::Peeked(Peeked::Folder { more, .. })) if *more > 0 => Some(("more", "hn.PeekMeta", None)),
            _ => None,
        };
        view::for_each_of(pane, head.into_iter().chain(body).chain(more), |v, pv, _| match pv {
            None if v.entity().name() == "head" => _ = v.text(p.map_or("", |p| p.path.as_str())),
            None => {
                if let Some(Preview::Peeked(Peeked::Folder { more, .. })) = &preview {
                    v.text(&format!("{more} more"));
                }
            }
            Some(Preview::Image { path, w: iw, h: ih }) => {
                // Drawn whole in the box, as large as fits.
                let k = (box_w / *iw as f32).min(box_h / *ih as f32).min(1.0);
                let (dw, dh) = ((*iw as f32 * k).round(), (*ih as f32 * k).round());
                let size = Size {
                    w: Sizing { kind: SizeKind::Fixed, value: dw, min: 0.0, max: 0.0 },
                    h: Sizing { kind: SizeKind::Fixed, value: dh, min: 0.0, max: 0.0 },
                };
                let e = v.entity();
                if e.try_cloned::<&Size>() != Some(size) {
                    e.set(size);
                }
                let pic = harness_native_ui_text_api::Picture { handle: path.as_str().into() };
                if e.try_cloned::<&harness_native_ui_text_api::Picture>() != Some(pic.clone()) {
                    e.set(pic);
                }
            }
            Some(Preview::Peeked(Peeked::Text(t))) => {
                v.text(t);
                let file = p.map_or("", |p| p.path.as_str());
                let e = v.entity();
                if harness_native_render::highlight::knows(file) {
                    if e.try_cloned::<&Highlight>().is_none_or(|h| h.file.as_str() != file) {
                        e.set(Highlight { file: file.into() });
                    }
                } else if e.has(Highlight::id()) {
                    e.remove(Highlight::id());
                }
            }
            Some(Preview::Peeked(Peeked::Note(n))) => _ = v.text(n),
            Some(Preview::Peeked(Peeked::Folder { entries, .. })) => {
                view::for_each(v, "hn.PeekEntry", entries.iter().map(|(k, d, n)| (k.as_str(), (*d, n.as_str()))), |r, (d, n), _| {
                    if let Some(i) = r.part("icon") {
                        i.drawing(d);
                    }
                    if let Some(l) = r.part("name") {
                        l.text(n);
                    }
                });
            }
        });
    }

    /// Whether chat `sid`'s mention menu is open now (its draft ends in an `@` word Escape did not close).
    fn mention_open(&self, sid: u64, e: u64) -> bool {
        let cwd = self.chat_cwd(e);
        let home = std::env::var("HOME").unwrap_or_default();
        let wake = self.link.waker();
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid) else { return false };
        st.pick.is_none() && st.mention.update(&st.draft, &cwd, &home, &wake)
    }

    /// A key while the focused chat's mention menu is open: up and down move its highlight, Tab, Return and Command-Return
    /// take it (Return with no row sends as usual), Escape closes it. Whether the menu took the key.
    pub(in crate::app) fn chat_mention_key(&mut self, op: &str) -> bool {
        let Some((_, e, sid)) = self.focused_chat() else { return false };
        if !self.mention_open(sid, e) {
            return false;
        }
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid) else { return false };
        let m = &mut st.mention;
        match op {
            "up" | "down" => m.nav.step(op == "down", m.rows.len(), |_| true),
            "interrupt" => m.shut = token(&st.draft).map(|(s, _)| s),
            "complete" | "send" | "submit" => {
                let Some(i) = m.nav.hot.filter(|&i| i < m.rows.len()) else {
                    return op == "complete";
                };
                drop(states);
                self.chat_mention_take(sid, i);
                return true;
            }
            _ => return false,
        }
        drop(states);
        self.poke();
        true
    }

    /// A press on row `rest` (`<sid>:<i>`) of a chat's mention menu.
    pub(in crate::app) fn chat_mention_action(&mut self, rest: &str) {
        let Some((sid, i)) = rest.split_once(':').and_then(|(s, i)| Some((s.parse().ok()?, i.parse().ok()?))) else {
            eprintln!("harness-native: mention: no chat and row in {rest:?}");
            return;
        };
        self.chat_mention_take(sid, i);
    }

    /// Chat `sid`'s draft takes mention row `i`: its `@` word becomes the file, one atom (a chip: its icon and name)
    /// and a space after it; a folder stays words, `@<path>/`, so the menu goes on into it.
    fn chat_mention_take(&mut self, sid: u64, i: usize) {
        {
            let mut states = self.chat.borrow_mut();
            let Some(st) = states.get_mut(&sid) else { return };
            let Some(p) = st.mention.rows.get(i).cloned() else { return };
            let Some((start, _)) = token(&st.draft) else { return };
            let d = &mut st.draft;
            d.anchor = start;
            if p.dir {
                d.insert(&format!("@{}", p.path));
            } else {
                d.insert_atom(&p.path);
                d.insert(" ");
            }
            st.mention.key = None;
            st.reading = false;
        }
        self.poke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(s: &str) -> Draft {
        let caret = s.find('|').expect("a caret");
        let text = s.replace('|', "");
        Draft { text, caret, anchor: caret, ..Draft::default() }
    }

    fn places(paths: &[&str]) -> Vec<Place> {
        paths.iter().map(|p| Place { path: (*p).to_owned(), dir: p.ends_with('/') }).collect()
    }

    #[test]
    fn an_at_word_at_the_caret_is_a_mention() {
        assert_eq!(token(&draft("@|")), Some((0, "")));
        assert_eq!(token(&draft("look at @src/ma|")), Some((8, "src/ma")));
        assert_eq!(token(&draft("look at @src|  next")), Some((8, "src")));
        assert_eq!(token(&draft("mail me@ix.dev|")), None, "an @ inside a word is not one");
        assert_eq!(token(&draft("@sr|c")), None, "the caret must end the word");
        assert_eq!(token(&draft("@src |")), None, "a blank ends it");
        let mut d = draft("@src|");
        d.anchor = 0;
        assert_eq!(token(&d), None, "not with a selection");
    }

    #[test]
    fn a_name_beats_a_path_and_a_start_beats_a_middle() {
        let p = places(&["docs/main/notes.md", "src/main.rs", "src/domain.rs", "src/", "docs/", "docs/main/"]);
        let r = rank(&p, "main");
        assert_eq!(r[0].path, "src/main.rs", "{r:?}");
        assert!(r.iter().position(|x| x.path == "src/domain.rs") > r.iter().position(|x| x.path == "docs/main/"), "{r:?}");
        assert_eq!(rank(&p, "zzz"), vec![]);
        let top = rank(&p, "");
        assert_eq!(top.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(), ["docs/", "src/"], "the directory's own, folders first");
        let p = places(&["harness-native-ui-state-api/", "rustfmt.toml", "icons/files/rust.svg", "harness-native-render/icons/files/ruby.svg"]);
        let r = rank(&p, "rus");
        assert_eq!(r.last().map(|x| x.path.as_str()), Some("harness-native-ui-state-api/"), "letters strewn about rank last: {r:?}");
    }

    #[test]
    fn git_files_bring_their_folders() {
        let p = from_git("a/b/c.rs\0a/d.rs\0e.md\0");
        let dirs: Vec<&str> = p.iter().filter(|x| x.dir).map(|x| x.path.as_str()).collect();
        assert_eq!(dirs, ["a/", "a/b/"]);
        assert_eq!(p.iter().filter(|x| !x.dir).count(), 3);
    }
}
