//! Command-O's local conversation library. One worker owns the index and source IO; the frame thread only consumes
//! snapshots. The mailbox coalesces progress and search results while the window is tucked away.
use super::*;
use harness_chat_library::{Hit, Index, Parsed, Roots};
use std::sync::{Mutex, mpsc};

const LIMIT: usize = 40;
const LIVE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

fn metadata_priority(query: &str, title: &str, ids: [&str; 2]) -> u8 {
    if query.is_empty() { return 3; }
    let title = title.to_lowercase();
    if title == query || ids.iter().any(|id| id.to_lowercase() == query) { return 0; }
    if title.starts_with(query) || ids.iter().any(|id| id.to_lowercase().starts_with(query)) { return 1; }
    if title.contains(query) || query.split_whitespace().all(|word| title.contains(word)) { return 2; }
    3
}

enum Command {
    Query(String),
    Refresh,
    Open { key: String, request: u64 },
    Watch(Option<String>),
}

#[derive(Default)]
struct Mail {
    state: Option<Snapshot>,
    opened: Option<ReadResult>,
    refreshed: Option<ReadResult>,
}

struct ReadResult {
    request: u64,
    key: String,
    result: Result<Parsed, String>,
}

struct Snapshot {
    query: String,
    hits: Vec<Hit>,
    progress: Option<(usize, usize)>,
    error: Option<String>,
    fatal: bool,
}

#[derive(Default)]
pub(super) struct Library {
    commands: Option<mpsc::SyncSender<Command>>,
    mail: Arc<Mutex<Mail>>,
    hits: Vec<Hit>,
    query: String,
    progress: Option<(usize, usize)>,
    error: Option<String>,
    watching: Option<String>,
    searching: bool,
    opening: bool,
    next_request: u64,
    pending_open: Option<u64>,
    source_errors: HashMap<String, String>,
    highlight: std::cell::RefCell<Option<String>>,
    worker_failed: bool,
    scroll: std::cell::RefCell<[view::field::FieldScroll; 2]>,
}

impl Library {
    fn start(&mut self, wake: Wake) {
        if self.worker_failed { return; }
        if self.commands.is_some() {
            self.send(Command::Refresh);
            return;
        }
        let (tx, rx) = mpsc::sync_channel(8);
        let mail = self.mail.clone();
        match std::thread::Builder::new().name("harness-chat-library".to_owned()).spawn(move || run(rx, mail, wake)) {
            Ok(_) => {
                self.commands = Some(tx);
                self.progress = Some((0, 0));
                self.send(Command::Refresh);
            }
            Err(e) => {
                self.error = Some(format!("Starting the chat library: {e}"));
                self.worker_failed = true;
            }
        }
    }

    fn send(&mut self, command: Command) {
        if self.worker_failed {
            self.searching = false;
            self.opening = false;
            return;
        }
        let Some(tx) = &self.commands else { return };
        if let Err(e) = tx.try_send(command) {
            self.error = Some(format!("The chat library could not accept the request: {e}"));
            self.searching = false;
            self.opening = false;
        }
    }

    pub(super) fn query(&mut self, query: &str) {
        self.query = query.to_owned();
        self.hits.clear();
        self.highlight.get_mut().take();
        *self.scroll.get_mut() = Default::default();
        self.searching = true;
        self.send(Command::Query(query.to_owned()));
    }

    pub(super) fn open(&mut self, key: &str) {
        let Some(request) = self.next_request.checked_add(1) else {
            self.error = Some("The chat library exhausted its request identities".to_owned());
            return;
        };
        self.next_request = request;
        self.pending_open = Some(request);
        self.opening = true;
        self.send(Command::Open { key: key.to_owned(), request });
    }
}

fn state(index: &Index, query: &str, progress: Option<(usize, usize)>, error: Option<String>, mail: &Mutex<Mail>, wake: &Wake) {
    let error = error.or_else(|| (!index.diagnostics().is_empty()).then(|| format!("{} chat source(s) need attention:\n{}", index.diagnostics().len(), index.diagnostics().join("\n"))));
    let (hits, error) = match index.search(query, LIMIT) {
        Ok(hits) => (hits, error),
        Err(e) => (Vec::new(), Some(e)),
    };
    mail.lock().unwrap_or_else(std::sync::PoisonError::into_inner).state = Some(Snapshot { query: query.to_owned(), hits, progress, error, fatal: false });
    wake();
}

fn stamp(path: &std::path::Path) -> Result<(u64, std::time::SystemTime), String> {
    let m = std::fs::metadata(path).map_err(|e| format!("Reading chat source {}: {e}", path.display()))?;
    Ok((m.len(), m.modified().map_err(|e| format!("Reading chat source time {}: {e}", path.display()))?))
}

fn open(index: &Index, key: &str, request: u64, changed_only: bool, seen: &mut HashMap<String, (u64, std::time::SystemTime)>, mail: &Mutex<Mail>, wake: &Wake) {
    // Read metadata before parsing so a source that grows during parsing is seen again on the next poll.
    let result = index.search(key, LIMIT).and_then(|hits| {
        let session = hits.into_iter().find(|h| h.session.key == key).map(|h| h.session)
            .ok_or_else(|| format!("The indexed chat {key} is no longer available"))?;
        let before = stamp(&session.path)?;
        if changed_only && seen.get(key) == Some(&before) {
            return Ok(None);
        }
        let parsed = index.read(key)?;
        seen.insert(key.to_owned(), before);
        Ok(Some(parsed))
    });
    let result = match result {
        Ok(None) => return,
        Ok(Some(parsed)) => Ok(parsed),
        Err(e) => Err(e),
    };
    let mut pending = mail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = ReadResult { request, key: key.to_owned(), result };
    if changed_only {
        pending.refreshed = Some(result);
    } else {
        pending.opened = Some(result);
    }
    drop(pending);
    wake();
}

fn run(rx: mpsc::Receiver<Command>, mail: Arc<Mutex<Mail>>, wake: Wake) {
    let init = Roots::user().and_then(|roots| {
        let path = roots.harness.join("cache/chat-library.sqlite");
        let dir = path.parent().ok_or("The chat index has no parent directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("Creating chat index directory {}: {e}", dir.display()))?;
        Index::open(&path).map(|index| (roots, index))
    });
    let (roots, mut index) = match init {
        Ok(v) => v,
        Err(e) => {
            mail.lock().unwrap_or_else(std::sync::PoisonError::into_inner).state = Some(Snapshot { query: String::new(), hits: Vec::new(), progress: None, error: Some(e), fatal: true });
            wake();
            return;
        }
    };
    let mut query = String::new();
    let mut seen = HashMap::new(); // alloc: source stamps of opened external chats, owned by this worker
    let mut watching: Option<String> = None;
    let mut polled = std::time::Instant::now();
    loop {
        // This service waits only for a UI request; dropping the UI's sender ends the worker.
        let command = match rx.recv_timeout(if watching.is_some() { LIVE_POLL } else { std::time::Duration::from_secs(60) }) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(key) = &watching {
                    open(&index, key, 0, true, &mut seen, &mail, &wake);
                    polled = std::time::Instant::now();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        match command {
            Command::Query(q) => {
                query = q;
                state(&index, &query, None, None, &mail, &wake);
            }
            Command::Open { key, request } => open(&index, &key, request, false, &mut seen, &mail, &wake),
            Command::Watch(key) => watching = key,
            Command::Refresh => {
                let result = index.refresh_with_progress(&roots, |index, done, total| {
                    for command in rx.try_iter().take(8) {
                        match command {
                            Command::Query(q) => query = q,
                            Command::Open { key, request } => open(index, &key, request, false, &mut seen, &mail, &wake),
                            Command::Watch(key) => watching = key,
                            // A refresh already in progress covers another request to refresh.
                            Command::Refresh => {}
                        }
                    }
                    if polled.elapsed() >= LIVE_POLL {
                        if let Some(key) = &watching {
                            open(index, key, 0, true, &mut seen, &mail, &wake);
                        }
                        polled = std::time::Instant::now();
                    }
                    let error = (!index.diagnostics().is_empty()).then(|| format!("{} chat source(s) need attention. {}", index.diagnostics().len(), index.diagnostics()[0]));
                    state(index, &query, Some((done, total)), error, &mail, &wake);
                });
                state(&index, &query, None, result.err(), &mail, &wake);
            }
        }
    }
}

impl App {
    pub(in crate::app) fn open_chat_library(&mut self) {
        self.library.pending_open = None;
        self.library.opening = false;
        if self.palette.as_ref().is_some_and(|p| p.mode == Mode::Chats) {
            self.palette = None;
            return;
        }
        self.library.start(self.link.waker());
        self.library.query("");
        self.palette = Some(Palette { mode: Mode::Chats, ..Palette::new(self.time) });
        self.poke();
    }

    pub(super) fn poll_chat_library(&mut self) {
        self.step_library_scrolling();
        let (snapshot, opened, refreshed) = {
            let mut mail = self.library.mail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (mail.state.take(), mail.opened.take(), mail.refreshed.take())
        };
        if let Some(s) = snapshot {
            if s.fatal || s.query == self.library.query {
                self.library.worker_failed |= s.fatal;
                self.library.hits = s.hits;
                self.library.progress = s.progress;
                self.library.error = s.error;
                self.library.searching = false;
            }
        }
        if let Some(read) = refreshed {
            match read.result {
                Ok(parsed) => {
                    self.library.source_errors.remove(&read.key);
                    self.refresh_external(parsed);
                }
                Err(e) => {
                    self.library.source_errors.insert(read.key, e);
                }
            }
        }
        if let Some(read) = opened.filter(|read| self.library.pending_open == Some(read.request)) {
            self.library.opening = false;
            self.library.pending_open = None;
            match read.result {
                Ok(parsed) => {
                    self.library.source_errors.remove(&read.key);
                    if self.palette.as_ref().is_some_and(|p| p.mode == Mode::Chats) {
                        self.palette = None;
                        self.open_external(parsed);
                    }
                }
                Err(e) => {
                    self.library.source_errors.insert(read.key, e.clone());
                    self.library.error = Some(e);
                }
            }
        }
        let key = self.selected().and_then(|e| self.session_of(e)).and_then(|sid| self.external_session(sid)).map(|s| s.key.clone());
        if key != self.library.watching {
            self.library.watching = key.clone();
            self.library.send(Command::Watch(key));
        }
    }

    fn library_body(&self) -> Option<View<'_>> {
        let (id, mode) = self.ids.palette?;
        (mode == Mode::Chats && self.palette.as_ref().is_some_and(|p| p.mode == Mode::Chats)).then_some(())?;
        View::of(self.world.entity_from_id(id)).part("card")?.part("body")
    }

    /// Overlay scrolls use the same native physics and overlay scroller as text areas; a selected row is the reveal target.
    fn step_library_scrolling(&mut self) {
        let Some(body) = self.library_body() else { return };
        let entries = self.library_entries();
        let at = self.library_selected(&entries, self.palette.as_ref().map_or(0, |p| p.at));
        let selected = entries.get(at).and_then(|row| body.part("results")?.part("items")?.part(&row.0));
        let line = selected.and_then(|r| r.entity().try_cloned::<&Laid>()).map(|l| [l.y, l.y + l.h]);
        let points_per_pixel = 1.0 / f64::from(self.grid.scale.max(0.5));
        let mut moving = false;
        {
            let mut scroll = self.library.scroll.borrow_mut();
            for (i, name) in ["results", "preview"].into_iter().enumerate() {
                if let Some(area) = body.part(name) { moving |= scroll[i].step(area.entity(), if i == 0 { line } else { None }, points_per_pixel); }
            }
        }
        if moving { self.poke(); }
    }

    /// A modal overlay owns the wheel, including at a scroll boundary; its underlying tree/chat never sees the gesture.
    pub(super) fn library_wheel(&mut self, ev: &sidebar::Wheel) -> bool {
        if self.palette.is_none() { return false; }
        let z = self.zoom();
        let took = self.library_body().and_then(|body| {
            ["results", "preview"].into_iter().enumerate().find_map(|(i, name)| {
                let area = body.part(name)?;
                let laid = area.entity().try_cloned::<&Laid>()?;
                laid.contains(self.pointer.x, self.pointer.y).then_some((i, area))
            })
        }).is_some_and(|(i, area)| {
            let k = if ev.precise { 1.0 / z } else { 1.0 };
            self.library.scroll.borrow_mut()[i].wheel(area.entity(), [ev.dx * k, ev.dy * k], ev.precise, ev.phase, ev.momentum)
        });
        if took { self.poke(); }
        true
    }

    pub(super) fn external_source_error(&self, key: &str) -> Option<&str> {
        self.library.source_errors.get(key).map(String::as_str)
    }

    pub(super) fn choose_library_key(&mut self, key: &str) {
        match self.library_entries().into_iter().find(|row| row.0 == key) {
            Some((.., action)) => self.choose(action),
            None => self.library.error = Some(format!("Chat {key} is no longer listed; search again")),
        }
    }

    pub(super) fn choose_library_highlight(&mut self) {
        let entries = self.library_entries();
        let at = self.library_selected(&entries, self.palette.as_ref().map_or(0, |p| p.at));
        if let Some((.., action)) = entries.into_iter().nth(at) {
            self.choose(action);
        }
    }

    pub(super) fn step_library(&mut self, by: i64) {
        let entries = self.library_entries();
        let at = self.library_selected(&entries, self.palette.as_ref().map_or(0, |p| p.at));
        if let Some(p) = self.palette.as_mut() {
            p.at = (at as i64 + by).clamp(0, entries.len().saturating_sub(1) as i64) as usize;
        }
        self.library.highlight.get_mut().take();
    }

    fn library_selected(&self, entries: &[(String, PalGlyph, String, String, Do)], at: usize) -> usize {
        self.library.highlight.borrow().as_ref().and_then(|key| entries.iter().position(|row| row.0 == *key))
            .unwrap_or_else(|| at.min(entries.len().saturating_sub(1)))
    }

    pub(super) fn library_entries(&self) -> Vec<(String, PalGlyph, String, String, Do)> {
        let query = self.palette.as_ref().map_or("", |p| p.query.text.trim());
        let mut ranked = Vec::new(); // alloc: only the open overlay's bounded results
        let mut ids = std::collections::HashSet::new(); // alloc: dedupe known Harness ids
        let q = query.to_lowercase();
        for hit in self.library.hits.iter().filter(|_| self.library.query.trim() == query) {
            let s = &hit.session;
            let entity = s.harness_id.and_then(|sid| self.object_for_session(sid));
            if let Some(sid) = s.harness_id {
                ids.insert(sid);
            }
            let priority = metadata_priority(&q, &s.title, [&s.chat_id, &s.key]);
            let recent = std::cmp::Reverse(if q.is_empty() { s.updated_ms } else { 0 });
            ranked.push((priority, recent, ranked.len(), (s.key.clone(), PalGlyph::Kind(s.agent.clone()), s.title.clone(), format!("{} · {}", capitalised(&s.agent), folder_of(&s.cwd)), entity.map_or_else(|| Do::OpenChat(s.key.clone()), Do::Select))));
        }
        // Current Harness chats are available immediately while a first external scan is still running.
        let mut local = Vec::new(); // alloc: current model objects, no transcript IO
        self.world.new_query::<&model::Object>().each_entity(|e, object| {
            if !e.has(model::ChatMode::id()) || ids.contains(&object.id) || self.external_session(object.id).is_some() {
                return;
            }
            let name = e.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            let id = format!("s{}", object.id);
            let key = format!("harness:{id}");
            let priority = metadata_priority(&q, &name, [&id, &key]);
            if !q.is_empty() && priority == 3 {
                return;
            }
            let cwd = e.try_cloned::<&model::Cwd>().map(|c| c.s.as_str().to_owned()).unwrap_or_default();
            local.push((model::when(e), priority, (key, PalGlyph::Kind(model::kind_name(e).to_owned()), name, format!("{} · {} · {id}", capitalised(model::kind_name(e)), folder_of(&cwd)), Do::Select(*e.id()))));
        });
        local.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.0.cmp(&b.2.0)));
        for (updated, priority, row) in local {
            ranked.push((priority, std::cmp::Reverse(if q.is_empty() { updated } else { 0 }), ranked.len(), row));
        }
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        ranked.into_iter().take(LIMIT).map(|(_, _, _, row)| row).collect()
    }

    fn object_for_session(&self, sid: u64) -> Option<u64> {
        let mut found = None;
        self.world.new_query::<&model::Object>().each_entity(|e, o| {
            if o.id == sid {
                found = Some(*e.id());
            }
        });
        found
    }

    pub(super) fn sync_chat_library(&self, card: Option<View<'_>>, at: usize, query: &str) {
        let Some(card) = card else { return };
        self.sync_palette_field(card, "Search chats…");
        let Some(body) = card.part("body") else { return };
        let Some(results) = body.part("results") else { return };
        let Some(items) = results.part("items") else { return };
        let entries = self.library_entries();
        let at = self.library_selected(&entries, at);
        let selected = entries.get(at);
        let key = selected.map(|row| row.0.clone());
        let changed = *self.library.highlight.borrow() != key;
        *self.library.highlight.borrow_mut() = key;
        let note = match (&self.library.error, self.library.progress, entries.is_empty()) {
            (Some(e), _, _) => format!("Needs attention · {}", e.lines().next().unwrap_or(e).chars().take(100).collect::<String>()),
            _ if self.library.opening => "Opening chat…".to_owned(),
            _ if self.library.searching || self.library.query.trim() != query.trim() => "Searching…".to_owned(),
            (_, Some((done, total)), _) => format!("Indexing · {done} / {total}"),
            (_, None, true) => "No matching chats".to_owned(),
            _ if query.is_empty() => "Recent chats".to_owned(),
            _ => format!("{} matches", entries.len()),
        };
        if let Some(label) = card.part("footer").and_then(|f| f.part("status")) {
            label.text(&note);
            label.set(harness_native_ui_input_api::Tooltip { text: self.library.error.as_deref().unwrap_or(&note).into() });
        }
        if let Some(empty) = results.part("empty") {
            empty.text(if !entries.is_empty() { "" } else if query.is_empty() { "Finding your local chats…" } else { "No chats found.\nTry a title, chat ID, or phrase." });
        }
        let w = self.world.world_ref();
        // The result page is bounded by LIMIT. Keep every row in the scroll content, so wheel, scroller and keys
        // use one coordinate space instead of replacing an eight-row slice beneath the pointer.
        view::for_each_of(items, entries.iter().enumerate().map(|(i, row)| (row.0.clone(), "hn.ChatLibraryRow", (i, row))), |row, (i, (key, glyph, title, meta, _)), _| {
            row.action(&format!("library:choose:{key}"));
            set_if(row, tone(w, if i == at { "selection" } else { "clear" }));
            if let Some(icon) = row.part("glyph") {
                if let PalGlyph::Kind(kind) = glyph { crate::look::kind_mark(w, icon.entity(), kind); }
            }
            if let Some(words) = row.part("words") {
                if let Some(name) = words.part("name") { name.text(&library_title(title)); }
                if let Some(label) = words.part("meta") { label.text(meta); }
            }
            if let Some(hit) = self.library.hits.iter().find(|h| h.session.key == *key) {
                row.set(harness_native_ui_input_api::Tooltip { text: format!("{}\n{}\n{}", hit.session.title, hit.session.chat_id, hit.session.path.display()).as_str().into() });
            }
        });
        if let Some(preview) = body.part("preview") {
            if changed {
                preview.set(harness_native_ui_layout_api::ScrollState::default());
                self.library.scroll.borrow_mut()[1] = Default::default();
            }
            if let Some(content) = preview.part("content") {
                let hit = selected.and_then(|row| self.library.hits.iter().find(|h| h.session.key == row.0));
                if let Some(source) = content.part("source") {
                    source.text(selected.map_or("Conversation preview", |row| row.3.as_str()));
                }
                if let Some(title) = content.part("title") { title.text(&selected.map_or_else(|| "Find a conversation".to_owned(), |row| library_title(&row.2))); }
                if let Some(identity) = content.part("identity") { identity.text(hit.map_or("", |h| h.session.chat_id.as_str())); }
                if let Some(excerpt) = content.part("excerpt") {
                    let text = hit.map_or_else(|| if selected.is_some() { "Open this chat to view its conversation." } else { "Search your Claude, Codex and Harness chats by title, ID, or anything said in the conversation." }, |h| h.snippet.as_str());
                    let mut paint = self.paint.borrow_mut();
                    let mut measure = |s: &str, style: &harness_native_ui_look_api::TextStyle, md: bool| paint.width(s, style, md);
                    chat::fill_markdown(excerpt, text, &mut measure);
                }
            }
        }
    }
}

/// Search titles are labels, not raw Markdown. A task's quoted name stands on its own; the full source stays in its tooltip.
fn library_title(source: &str) -> String {
    let source = source.trim();
    let task = source.strip_prefix("Task:").map(str::trim);
    let source = task.and_then(|t| t.strip_prefix('"').and_then(|t| t.split_once('"').map(|(name, _)| name))).unwrap_or(source);
    let source = source.trim_start_matches('#').trim();
    harness_native_render::markdown::inline(source).text.split_whitespace().collect::<Vec<_>>().join(" ")
}
