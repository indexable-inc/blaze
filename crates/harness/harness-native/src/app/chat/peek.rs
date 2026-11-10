//! Inline components under the pointer and what they point at (scene/widgets/peek.flecs, harness-native-ui-prims'
//! inline.flecs):
//!
//! - Hover: the chip under the pointer (a `<Link>`, `<Path>`, `<PR>`, `<Issue>`) gets `InlineHot` on its words' node,
//!   so the painter draws its plate brighter and underlines its label (pressed: brighter still), and the pointer turns
//!   to the pointing hand ([`App::chip_pointer`]).
//! - Hover card: after the pointer rests on a chip `inline_peek_dwell` seconds, an `hn.ChatPeek` (a ui.Menu, the shared
//!   popover) opens against an anchor laid over the chip, until the pointer leaves the chip and the card both. A pull
//!   request or issue shows its title, state, checks, author, lines changed and description's start (the batched
//!   `github_status` answer); a web link its OpenGraph card (`page_peek`, the daemon fetches and keeps it) with a
//!   [`PageView`] seam on the card for an embedded browser's live view; a file its first lines, a folder its entries the
//!   Finder's way (folders first, each with its kind's icon). Every card is a header row (the thing's icon, or a page's
//!   favicon, and where it is: `owner/repo #n`, the site, the path), the title flush left, then what there is; a line
//!   with nothing to show is left out, never laid out empty.
//! - Live state: every `<PR>` and `<Issue>` in a shown chat is asked after in one `github_status` request when the chat
//!   is focused (past the daemon's cache), when its words name new ones, and every [`REASK`] seconds while it shows; the
//!   answers become `InlineLive` entities under `ui.inline.live` with `(InlineState, <state child>)` and
//!   `(InlineMark, <mark>)`, which pick the chip's icon, ink and CI mark. A failed fetch leaves the chip in its unknown
//!   state with the error, naming the repository and number, as its card's words and in the log.
//! - Checks: an open pull request card whose rollup says running or failed asks after its checks one by one
//!   (`github_checks`, [`App::ask_checks`]: as it opens, then every [`REASK`] seconds while one is unfinished). Its checks
//!   line then says what is left (`2 of 9 running · 4m`, `1 failed`: chat/checks.rs), its mark breathing while anything
//!   is unfinished (the frames that keep coming tick the times); with the pointer on that line, a list opens under it: a
//!   row each check not passed (failed, running, queued), its step in progress and its time, and one row the rest fold
//!   into. A row's press opens its log in the harness browser (`web:open:`, app/browser.rs).
//! - Links: every `<Link>` in a shown chat (a link to a pull request or issue is that component already, harness-native-
//!   ui-inline's `from_url`) has its page's card asked for once (`page_peek`, its favicon with it), at most
//!   [`PAGES_MAX`] a chat, so its chip shows the favicon at rest (`InlineLive.picture`), the globe without one.
//! - Places: every `<Path>` and `<FileRef>` of a shown chat that names one place wherever it is said (absolute, or from
//!   `~`) is looked at once as its words change (the `fs` feed), so its chip's icon is a folder's when it is a folder
//!   whatever its name says, and a place that is not there says why as its tooltip ([`App::sync_places`]).
use super::*;
use harness_native_render::inline::{self as vocab, ChipIcon};
use harness_native_render::markdown::{CHIP_INSET_EM, Inline};
use harness_native_render::paint::spans_of;
use harness_native_ui_inline::{Tag, files};
use harness_native_ui_look_api::Fill;
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{Highlight, Icon, InlineHot, InlineLive, InlineMark, InlineState, PageView, Picture};
use super::checks;
use harness_proto::peek::{GhCheckState, GhChecks, GhCi, GhItem, GhKind, GhRef, GhState, PageMeta};
use std::collections::BTreeMap;

/// How often a shown chat asks again after its pull requests' state (seconds; the daemon keeps answers a minute).
const REASK: f32 = 60.0;
/// The most of a file a path's card reads.
const FILE_PEEK_BYTES: u64 = 16 << 10;
/// The most links of one chat whose pages (favicons) are asked for.
const PAGES_MAX: usize = 64;

/// A chip under the pointer: the words' node, which of its components, the tag, and its rectangle (window points).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Chip {
    node: u64,
    index: usize,
    tag: Tag,
    rect: [f32; 4],
}

/// One chat's hover and asking state (in its ChatState).
#[derive(Default)]
pub(super) struct Peek {
    /// The chip under the pointer and since when (the pane clock's seconds); the chip whose card is open.
    hot: Option<(Chip, f32)>,
    open: Option<Chip>,
    /// The transcript revision whose refs were last gathered, those refs, when they were last asked, and whether the
    /// pane had the keyboard then.
    rev: Option<(u64, usize)>,
    refs: Vec<GhRef>,
    urls: Vec<String>,
    asked: Option<f32>,
    focused: bool,
    /// The pull request whose checks one by one the open card last asked after, and when (cleared as the card goes, so
    /// each opening asks again past the daemon's few seconds' cache).
    checks_asked: Option<(GhRef, f32)>,
    /// What a path's card shows of its place, by target, read once.
    files: BTreeMap<String, Peeked>,
}

/// The app's side of live components: each GitHub ref's and each link's page's key in the scene's vocabulary, and the
/// link's answers' revision last written to the world.
#[derive(Default)]
pub(crate) struct Live {
    keys: BTreeMap<GhRef, String>,
    pages: BTreeMap<String, String>,
    rev: u64,
}

/// A tag's web page and its live key, for a component the scene keeps by the `page` feed (a link).
fn page_ref(w: WorldRef<'_>, tag: &Tag) -> Option<(String, String)> {
    let (feed, key) = vocab::keyed(w, tag)?;
    if feed != "page" {
        return None;
    }
    let url = vocab::target(w, tag, "/", "/").ok().filter(|u| u.starts_with("https://") || u.starts_with("http://"))?;
    Some((url, key))
}

/// A tag's GitHub ref and its live key, for a component the scene keeps by a `github.*` feed.
fn gh_ref(w: WorldRef<'_>, tag: &Tag) -> Option<(GhRef, String)> {
    let (feed, key) = vocab::keyed(w, tag)?;
    let kind = match feed.as_str() {
        "github.pr" => GhKind::Pr,
        "github.issue" => GhKind::Issue,
        _ => return None,
    };
    let repo = tag.prop("repo").ok()?;
    let n = tag.prop("n").ok()?.parse::<u64>().ok()?;
    Some((GhRef { kind, repo, n }, key))
}

/// A state's child name under its component (`ui.inline.PR.merged`).
fn state_name(s: GhState) -> &'static str {
    match s {
        GhState::Open => "open",
        GhState::Draft => "draft",
        GhState::Merged => "merged",
        GhState::Closed => "closed",
        GhState::NotPlanned => "not_planned",
    }
}

fn ci_name(c: GhCi) -> &'static str {
    match c {
        GhCi::Success => "success",
        GhCi::Failure => "failure",
        GhCi::Pending => "pending",
    }
}

impl App {
    /// The pointer over a chip of the focused chat: the pointing hand (the pointer module's order puts it after drags).
    pub(crate) fn chip_pointer(&self) -> Option<(Pointer, &'static str)> {
        let (_, _, sid) = self.focused_chat()?;
        self.chat.borrow().get(&sid)?.peek.hot.as_ref().map(|_| (Pointer::Hand, "inline chip"))
    }

    /// A chat pane's frame for its chips (from `fill_chat_pane`): ask after its pull requests and issues, write what the
    /// link knows into the world, find the chip under the pointer and keep its card.
    pub(super) fn fill_chat_peek(&self, pv: View<'_>, st: &mut ChatState, t: &Transcript, rows: &[(String, Row)], focused: bool, cwd: &str) {
        let w = self.world.world_ref();
        self.ask_github(w, st, t, rows, focused);
        self.sync_live(w);
        // The card first: the pointer on it is on no chip under it.
        let slot = pv.part("peek");
        let on_card = slot.and_then(|s| s.part("card")).is_some_and(|card| under_pointer(w, card));
        let chip = if focused && !on_card { self.chip_under(pv, rows) } else { None };
        // The hot chip: its node's InlineHot, kept since it was first under the pointer.
        let was = st.peek.hot.as_ref().map(|(c, _)| (c.node, c.index));
        let now = chip.as_ref().map(|c| (c.node, c.index));
        if was != now {
            if let Some((node, _)) = was {
                let e = w.entity_from_id(node);
                if e.is_alive() && e.has(InlineHot::id()) {
                    e.remove(InlineHot::id());
                }
            }
            st.peek.hot = chip.clone().map(|c| (c, self.time));
        } else if let (Some((c, _)), Some(new)) = (st.peek.hot.as_mut(), chip.clone()) {
            c.rect = new.rect;
        }
        if let Some((c, _)) = &st.peek.hot {
            let hot = InlineHot { tag: c.index as u32, pressed: self.pointer.down };
            let e = w.entity_from_id(c.node);
            if e.try_cloned::<&InlineHot>() != Some(hot) {
                e.set(hot);
            }
        }
        // The card: open after the dwell, kept while the pointer is on the chip or the card.
        let dwell = crate::look::width(w, "inline_peek_dwell");
        match &st.peek.hot {
            Some((c, since)) if self.time - since >= dwell && !self.pointer.down => st.peek.open = Some(c.clone()),
            // Resting, not yet long enough: frames keep coming until it is; a card open on another chip goes.
            Some((c, _)) if !self.pointer.down => {
                if st.peek.open.as_ref().is_some_and(|o| (o.node, o.index) != (c.node, c.index)) {
                    st.peek.open = None;
                }
                moving(&w);
            }
            Some(_) => st.peek.open = None,
            None if on_card => {}
            None => st.peek.open = None,
        }
        if let Some(open) = &mut st.peek.open
            && let Some((c, _)) = st.peek.hot.as_ref().filter(|(c, _)| c.node == open.node && c.index == open.index)
        {
            open.rect = c.rect;
        }
        if st.peek.open.is_none() {
            st.peek.checks_asked = None;
        }
        let Some(slot) = slot else { return };
        let open = st.peek.open.clone();
        let shown = open.as_ref().map(|_| [("anchor", "hn.ChatPeekAnchor", ()), ("card", "hn.ChatPeek", ())]);
        view::for_each_of(slot, shown.into_iter().flatten(), |_, (), _| {});
        let (Some(c), Some(anchor), Some(card)) = (open, slot.part("anchor"), slot.part("card")) else { return };
        let laid = Laid { x: c.rect[0], y: c.rect[1], w: c.rect[2], h: c.rect[3] };
        if anchor.entity().try_cloned::<&Laid>() != Some(laid) {
            anchor.set(laid);
        }
        card.anchored(anchor.id());
        self.fill_card(card, st, &c.tag, cwd, false);
    }

    /// Gather the chat's GitHub refs, links and places when its words change, and ask after the refs: fresh as the chat
    /// takes the keyboard, when there are new ones, and every [`REASK`] seconds while it shows. Its places are looked at
    /// as they are gathered ([`App::sync_places`]).
    fn ask_github(&self, w: WorldRef<'_>, st: &mut ChatState, t: &Transcript, rows: &[(String, Row)], focused: bool) {
        let p = &mut st.peek;
        let mut new = false;
        // Its words move with the transcript and with what is folded (the rows shown).
        if p.rev != Some((t.rev, rows.len())) {
            p.rev = Some((t.rev, rows.len()));
            let mut refs: Vec<GhRef> = Vec::new(); // alloc: this chat's refs, kept with its state
            let mut urls: Vec<String> = Vec::new(); // alloc: this chat's links, kept with its state
            let mut places: Vec<(String, String, EntityView<'_>)> = Vec::new(); // alloc: this pass's places
            let mut live = self.peek_live.borrow_mut();
            for (_, r) in rows {
                // A card's component is asked after as a chip's is.
                let card = match r {
                    Row::RefCard(src) => Some(Words::at("", src.clone(), true)),
                    _ => None,
                };
                for Words { src, md, .. } in texts(r).into_iter().chain(card) {
                    if !md {
                        continue;
                    }
                    for tag in harness_native_render::markdown::inline(&src).tags {
                        if let Some((gh, key)) = gh_ref(w, &tag.tag) {
                            live.keys.insert(gh.clone(), key);
                            if !refs.contains(&gh) {
                                refs.push(gh);
                            }
                        } else if let Some((url, key)) = page_ref(w, &tag.tag)
                            && urls.len() < PAGES_MAX
                            && !urls.contains(&url)
                        {
                            live.pages.insert(url.clone(), key);
                            urls.push(url);
                        } else if let Some(place) = vocab::place(w, &tag.tag)
                            && !places.iter().any(|(k, _, _)| *k == place.0)
                        {
                            places.push(place);
                        }
                    }
                }
            }
            new = refs.iter().any(|r| !p.refs.contains(r));
            p.refs = refs;
            // Each page once (the link keeps what it was told): its chip's favicon, its card ready for a hover.
            let asks: Vec<&str> = urls.iter().map(String::as_str).collect(); // alloc: this pass's links, borrowed
            self.link.pages(&asks);
            p.urls = urls;
            drop(live);
            self.sync_places(w, &places);
        }
        let fresh = focused && !p.focused;
        p.focused = focused;
        let due = p.asked.is_none_or(|at| self.time - at >= REASK);
        if !p.refs.is_empty() && (fresh || new || due) {
            p.asked = Some(self.time);
            self.link.github(p.refs.clone(), fresh);
        }
    }

    /// Write the link's GitHub answers into `ui.inline.live` when they moved: one `InlineLive` a ref, its state and mark
    /// pairs to the scene's entities (none while unknown).
    fn sync_live(&self, w: WorldRef<'_>) {
        let peeks = self.link.peeks();
        let mut live = self.peek_live.borrow_mut();
        if peeks.rev == live.rev {
            return;
        }
        live.rev = peeks.rev;
        let Some(root) = w.try_lookup(vocab::LIVE) else { panic!("the scene has no {} (inline.flecs)", vocab::LIVE) };
        let mut have: BTreeMap<String, EntityView<'_>> = BTreeMap::new(); // alloc: the live entities, this pass
        root.each_child(|c| {
            if let Some(l) = c.try_cloned::<&InlineLive>() {
                have.insert(l.key.as_str().to_owned(), c);
            }
        });
        for (r, it) in &peeks.gh {
            let Some(key) = live.keys.get(r) else { continue };
            let e = match have.get(key) {
                Some(e) => *e,
                None => w.entity().child_of(*root.id()),
            };
            let tip = if it.error.is_empty() { it.title.clone() } else { it.error.clone() };
            let l = InlineLive { key: key.as_str().into(), tip: tip.as_str().into(), picture: "".into() };
            if e.try_cloned::<&InlineLive>() != Some(l.clone()) {
                e.set(l);
            }
            let component = match r.kind {
                GhKind::Pr => "PR",
                GhKind::Issue => "Issue",
            };
            let state = it.state.and_then(|s| w.try_lookup(&format!("{}::{component}::{}", vocab::MODULE, state_name(s))));
            let mark = it.ci.and_then(|c| w.try_lookup(&format!("{}::marks::{}", vocab::MODULE, ci_name(c))));
            for (rel, to) in [(*w.component::<InlineState>().id(), state), (*w.component::<InlineMark>().id(), mark)] {
                let cur = e.target(rel, 0).map(|t| *t.id());
                match to {
                    Some(t) if cur != Some(*t.id()) => _ = e.add((rel, *t.id())),
                    None if cur.is_some() => _ = e.remove((rel, flecs::Wildcard::ID)),
                    _ => {}
                }
            }
        }
        // Each link's page: its title (or error) to say, its favicon to show in its icon's place.
        for (url, page) in &peeks.pages {
            let Some(key) = live.pages.get(url) else { continue };
            let e = match have.get(key) {
                Some(e) => *e,
                None => w.entity().child_of(*root.id()),
            };
            let tip = if page.error.is_empty() { page.title.as_str() } else { page.error.as_str() };
            let l = InlineLive { key: key.as_str().into(), tip: tip.into(), picture: page.icon.as_str().into() };
            if e.try_cloned::<&InlineLive>() != Some(l.clone()) {
                e.set(l);
            }
        }
        moving(&w);
    }

    /// Write what the file system says of each place into `ui.inline.live` (the `fs` feed): its component's `folder` or
    /// `file` state, or, for one that is not there, no state and why as its tooltip, naming it. Only a place that is the
    /// same wherever it is said (absolute, or from `~`) is kept: its key is the place as written, and a relative one is
    /// another place in each session's directory, so its chip keeps the icon its name says.
    fn sync_places(&self, w: WorldRef<'_>, places: &[(String, String, EntityView<'_>)]) {
        let home = std::env::var("HOME").unwrap_or_default();
        let fixed = places.iter().filter(|(_, loc, _)| loc.starts_with('/') || loc == "~" || loc.starts_with("~/"));
        let mut fixed = fixed.peekable();
        if fixed.peek().is_none() {
            return;
        }
        let Some(root) = w.try_lookup(vocab::LIVE) else { panic!("the scene has no {} (inline.flecs)", vocab::LIVE) };
        let mut have: BTreeMap<String, EntityView<'_>> = BTreeMap::new(); // alloc: the live entities, this pass
        root.each_child(|c| {
            if let Some(l) = c.try_cloned::<&InlineLive>() {
                have.insert(l.key.as_str().to_owned(), c);
            }
        });
        let rel = *w.component::<InlineState>().id();
        for (key, loc, component) in fixed {
            let Ok(path) = harness_native_ui_inline::open_target(loc, None, "/", &home) else { continue };
            let (state, tip) = match std::fs::metadata(&path) {
                Ok(m) => (Some(if m.is_dir() { "folder" } else { "file" }), String::new()),
                Err(e) => (None, format!("{path}: {e}")),
            };
            let to = state.map(|s| {
                let Some(t) = component.lookup_child(s) else { panic!("{} has no `{s}` state (inline.flecs)", component.path()) };
                *t.id()
            });
            let e = match have.get(key) {
                Some(e) => *e,
                None => w.entity().child_of(*root.id()),
            };
            let l = InlineLive { key: key.as_str().into(), tip: tip.as_str().into(), picture: "".into() };
            if e.try_cloned::<&InlineLive>() != Some(l.clone()) {
                e.set(l);
            }
            let cur = e.target(rel, 0).map(|t| *t.id());
            match to {
                Some(t) if cur != Some(t) => _ = e.add((rel, t)),
                None if cur.is_some() => _ = e.remove((rel, flecs::Wildcard::ID)),
                _ => {}
            }
        }
        moving(&w);
    }

    /// The chip under the pointer in chat pane `pv`'s transcript, when there is one.
    fn chip_under(&self, pv: View<'_>, rows: &[(String, Row)]) -> Option<Chip> {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let list = pv.part("list")?;
        if !list.entity().try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y)) {
            return None;
        }
        let stack = list.part("rows")?;
        let mut paint = self.paint.borrow_mut();
        let mut found = None;
        for (key, r) in rows {
            if found.is_some() {
                break;
            }
            let Some(row) = stack.part(key) else { continue };
            if !row.entity().try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y)) {
                continue;
            }
            for t in texts(r) {
                if !t.md || !t.src.contains('<') {
                    continue;
                }
                let Some(node) = t.node(row) else { continue };
                let src = t.src;
                let Some((lay, l)) = words_layout(&mut paint, node.entity()) else { continue };
                if !l.contains(x, y) {
                    continue;
                }
                let text = node.entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or(src);
                let m: Inline = harness_native_render::markdown::inline(&text);
                let size = node.entity().try_cloned::<&TextStyle>().unwrap_or_default().size;
                let inset = size * CHIP_INSET_EM;
                for (index, t) in m.tags.iter().enumerate() {
                    let hit = spans_of(&lay, t.start, t.end).into_iter().find(|r| {
                        let (x0, y0) = (l.x + r[0] - inset, l.y + r[1]);
                        x >= x0 && x < x0 + r[2] + 2.0 * inset && y >= y0 && y < y0 + r[3]
                    });
                    if let Some(r) = hit {
                        let rect = [l.x + r[0] - inset, l.y + r[1], r[2] + 2.0 * inset, r[3]];
                        found = Some(Chip { node: node.id(), index, tag: t.tag.clone(), rect });
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
        }
        found
    }

    /// The card's lines for `tag`: what the link knows of its GitHub item, its page, or the file or folder it names (a
    /// relative one from `cwd`, the session's directory). A link's card carries its page's [`PageView`] (the live view's
    /// seam), and its live view line only once a browser host made one.
    /// Card `card` for component `tag`: a hover card, or (`block`) a card in the transcript, where the checks not passed
    /// are listed without the pointer and more of the description shows.
    pub(super) fn fill_card(&self, card: View<'_>, st: &mut ChatState, tag: &Tag, cwd: &str, block: bool) {
        let w = self.world.world_ref();
        let dark = self.chat_dark();
        let mut lines: Vec<(&'static str, &'static str, Line)> = Vec::new(); // alloc: a handful of lines, while it is up
        let mut page_view = None;
        if let Some((r, _)) = gh_ref(w, tag) {
            let (item, checks) = {
                let p = self.link.peeks();
                (p.gh.get(&r).cloned(), p.checks.get(&r).cloned())
            };
            self.ask_checks(st, &r, item.as_ref(), checks.as_ref());
            // The checks' list opens while the pointer is on the state line or the list (one hit target, PeekState).
            // Listed at once on a card in the transcript, or whenever anything failed (what failed is the news); else
            // while the pointer is on the state line.
            let failed = item.as_ref().is_some_and(|it| it.ci == Some(GhCi::Failure))
                || checks.as_ref().is_some_and(|c| c.checks.iter().any(|k| k.state == GhCheckState::Failed));
            let open = block || failed || card.part("state").is_some_and(|s| under_pointer(w, s));
            let at = Checks { known: checks.as_ref(), open, now: unix_now(), expand: block };
            gh_lines(w, tag, &r, item.as_ref(), &at, dark, &mut lines);
        } else {
            let home = std::env::var("HOME").unwrap_or_default();
            match vocab::target(w, tag, cwd, &home) {
                Ok(url) if url.starts_with("https://") || url.starts_with("http://") => {
                    self.link.page(&url);
                    let page = self.link.peeks().pages.get(&url).cloned();
                    let globe = vocab::icon(w, tag, dark);
                    let view = card.entity().try_cloned::<&PageView>().filter(|v| v.url.as_str() == url);
                    page_lines(&url, page.as_ref(), &globe, view.as_ref(), &mut lines);
                    page_view = Some(view.unwrap_or(PageView { url: url.as_str().into(), key: 0, ready: false }));
                }
                Ok(target) => {
                    let n = crate::look::width(w, "inline_peek_lines") as usize;
                    let entries = crate::look::width(w, "inline_peek_entries") as usize;
                    let peeked = st.peek.files.entry(target.clone()).or_insert_with(|| peek_place(&target, n, entries));
                    let path = target.split_once(':').filter(|(n, _)| n.parse::<u32>().is_ok()).map_or(target.as_str(), |(_, p)| p);
                    // Its icon by what the file system said (a folder's whatever its name), the name's when it said nothing.
                    let kind = match peeked {
                        Peeked::Folder { .. } => files::kind_at(path, true),
                        Peeked::Text(_) | Peeked::Note(_) => files::kind_of(path),
                    };
                    lines.push(header(ChipIcon::Drawing(files::drawing(kind)), None, path.to_owned()));
                    match peeked {
                        // A markdown file's lines read as the chat reads markdown (the transcript's own blocks and
                        // inline faces), never as its source; any other file's as code.
                        Peeked::Text(text) if matches!(files::kind_of(path), "markdown") => {
                            lines.push(("body", "hn.PeekMarkdown", Line::Markdown(text.clone())));
                        }
                        Peeked::Text(text) => {
                            lines.push(("code", "hn.PeekCode", Line::Code { text: text.clone(), file: path.to_owned() }));
                        }
                        Peeked::Note(note) => lines.push(("why", "hn.PeekMeta", Line::Words(note.clone()))),
                        Peeked::Folder { entries, more } => {
                            lines.push(("entries", "hn.PeekList", Line::Entries(entries.clone())));
                            if *more > 0 {
                                lines.push(("more", "hn.PeekMeta", Line::Words(format!("{more} more"))));
                            }
                        }
                    }
                }
                Err(why) => lines.push(("why", "hn.PeekMeta", Line::Words(why))),
            }
        }
        // The live view's seam, on the card: a link's page; nothing on any other card.
        match page_view {
            Some(v) => set_if(card, v),
            None if card.entity().has(PageView::id()) => _ = card.entity().remove(PageView::id()),
            None => {}
        }
        let mut markdown = None;
        view::for_each_of(card, lines.iter().map(|(k, p, l)| (*k, *p, l)), |v, l, _| match l {
            Line::Words(s) | Line::Title(s) => {
                v.text(s);
            }
            Line::Markdown(s) => markdown = Some((v, s.clone())),
            // Code as the editor shows it: coloured by its language (tree-sitter, `code_ink.*`), each line whole and cut by
            // the card's edge, never wrapped onto the next (a wrapped line lost its indent and read as dedented).
            Line::Code { text, file } => {
                if let Some(c) = v.part("text") {
                    c.text(text);
                    let e = c.entity();
                    if harness_native_render::highlight::knows(file) {
                        if e.try_cloned::<&Highlight>().is_none_or(|h| h.file.as_str() != file.as_str()) {
                            e.set(Highlight { file: file.as_str().into() });
                        }
                    } else if e.has(Highlight::id()) {
                        e.remove(Highlight::id());
                    }
                }
            }
            Line::Site { glyph, ink, name } => {
                if let Some(i) = v.part("icon") {
                    set_if(i, Icon { glyph: glyph.as_str().into() });
                    if let Some(f) = ink {
                        set_if(i, *f);
                    }
                }
                if let Some(n) = v.part("name") {
                    n.text(name);
                }
            }
            Line::Art { drawing, name } => {
                if let Some(i) = v.part("icon") {
                    i.drawing(drawing);
                }
                if let Some(n) = v.part("name") {
                    n.text(name);
                }
            }
            Line::Entries(entries) => {
                view::for_each(v, "hn.PeekEntry", entries.iter().map(|(k, d, n)| (k.as_str(), (*d, n.as_str()))), |r, (d, n), _| {
                    if let Some(i) = r.part("icon") {
                        i.drawing(d);
                    }
                    if let Some(l) = r.part("name") {
                        l.text(n);
                    }
                });
            }
            Line::Favicon { picture, name } => {
                if let Some(i) = v.part("icon") {
                    set_if(i, Picture { handle: picture.as_str().into() });
                }
                if let Some(n) = v.part("name") {
                    n.text(name);
                }
            }
            Line::State { label, ink, mark, checks, running, rows } => {
                let top = v.part("top");
                if let Some(b) = top.and_then(|t| t.part("badge")) {
                    set_if(b, *ink);
                    if let Some(l) = b.part("label") {
                        l.text(label);
                    }
                }
                if let Some(m) = top.and_then(|t| t.part("mark")) {
                    let (g, f) = mark.clone().unwrap_or_default();
                    set_if(m, Icon { glyph: g.as_str().into() });
                    set_if(m, f);
                    breathe(m, *running);
                }
                if let Some(c) = top.and_then(|t| t.part("checks")) {
                    c.text(checks);
                }
                if let Some(list) = v.part("list") {
                    view::for_each_of(list, rows.iter().map(|(k, p, r)| (k.as_str(), *p, r)), |rv, r, _| fill_check(rv, r));
                }
            }
            Line::Image(path) => set_if(v, Picture { handle: path.as_str().into() }),
            Line::Page(key) => set_if(v, harness_native_ui_text_api::Surface { key: *key, cover: 1 }),
        });
        // The description as the transcript draws markdown: a row a block (a paragraph, a list item, a heading, code),
        // each filled as a transcript row is.
        if let Some((v, md)) = markdown {
            let mut rows = Vec::new();
            super::blocks(&md, false, &mut rows);
            let mut paint = self.paint.borrow_mut();
            let mut measure = |s: &str, style: &TextStyle, md: bool| paint.width(s, style, md);
            // In the card's type: its body size and line (hn.PeekBody's), a step under its title, for every block's words.
            let body = view::prefab(w, "hn.PeekBody").and_then(|p| p.try_cloned::<&TextStyle>());
            view::for_each_of(v, rows.iter().enumerate().map(|(k, r)| (format!("b{k}"), r.prefab(), r)), |row, r, _| {
                super::fill_row(row, r, &super::RowCtx::NOWHERE, &mut measure);
                if let Some(b) = body {
                    card_type(row.entity(), b);
                }
            });
        }
    }

    /// A pull request's open card asks after its checks one by one while the rollup says some run or failed: once as it
    /// opens, then every [`REASK`] seconds while a check is unfinished (or the rollup still says running, a push GitHub
    /// has yet to list), and never for one whose checks all passed. One pull request at a time, only while its card
    /// shows: the daemon's answer is bounded by [`harness_proto::peek::CHECKS_MAX`] checks.
    fn ask_checks(&self, st: &mut ChatState, r: &GhRef, it: Option<&GhItem>, known: Option<&GhChecks>) {
        let Some(it) = it.filter(|it| r.kind == GhKind::Pr && it.error.is_empty()) else { return };
        if !matches!(it.ci, Some(GhCi::Pending | GhCi::Failure)) {
            return;
        }
        let stale = it.ci == Some(GhCi::Pending) || known.is_none_or(|c| c.unfinished() || !c.error.is_empty());
        let due = match &st.peek.checks_asked {
            Some((asked, at)) if asked == r => stale && self.time - at >= REASK,
            _ => stale || known.is_none(),
        };
        if due {
            st.peek.checks_asked = Some((r.clone(), self.time));
            self.link.checks(r.clone());
        }
    }
}

/// Whether the pointer is on `v` or something inside it (the app's `Hovered`, a hit node, and its ancestors).
fn under_pointer(w: WorldRef<'_>, v: View<'_>) -> bool {
    let id = v.id();
    w.entity_named(APP)
        .target(Hovered::id(), 0)
        .is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == id))
}

/// Seconds since the Unix epoch, the clock GitHub's check times are on.
fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// What a card knows of a pull request's checks one by one, whether its list is open, and the time to say them at.
struct Checks<'a> {
    known: Option<&'a GhChecks>,
    open: bool,
    now: i64,
    /// A card in the transcript (not a hover card): more of the description.
    expand: bool,
}

/// One row of the checks' list.
enum CheckRow {
    /// A check not passed: its name, the step it is on, its time, the words of its tooltip, and its press's action.
    Check { name: String, step: String, time: String, tip: String, action: String },
    /// The passed and skipped ones, folded (`7 passed`).
    Folded(String),
    /// Why its checks could not be read, naming the pull request.
    Why(String),
}

/// A check's row's prefab: its mark by its state.
fn check_prefab(s: GhCheckState) -> &'static str {
    match s {
        GhCheckState::Failed => "hn.PeekCheckFailed",
        GhCheckState::Running => "hn.PeekCheckRunning",
        GhCheckState::Queued | GhCheckState::Passed | GhCheckState::Skipped => "hn.PeekCheckQueued",
    }
}

/// The list's rows for `c` at `now`: a row each check not passed (in [`checks::listed`]'s order, keyed by its place in
/// the answer so a row keeps its node while its state moves), then the folded rest; or why there are none.
fn check_rows(c: &GhChecks, now: i64) -> Vec<(String, &'static str, CheckRow)> {
    if !c.error.is_empty() {
        return vec![("why".to_owned(), "hn.PeekMeta", CheckRow::Why(c.error.clone()))];
    }
    let mut rows: Vec<(String, &'static str, CheckRow)> = checks::listed(&c.checks)
        .into_iter()
        .map(|(j, k)| {
            let step = if k.state == GhCheckState::Running { k.step.clone() } else { String::new() };
            let tip = [k.workflow.as_str(), k.name.as_str(), step.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            let row = CheckRow::Check {
                name: k.name.clone(),
                step,
                time: checks::elapsed(k, now).map(checks::clock).unwrap_or_default(),
                tip,
                action: if k.url.is_empty() { String::new() } else { format!("web:open:{}", k.url) },
            };
            (format!("c{j}"), check_prefab(k.state), row)
        })
        .collect();
    if let Some(words) = checks::folded(&c.checks, c.total) {
        rows.push(("rest".to_owned(), "hn.PeekChecksFolded", CheckRow::Folded(words)));
    }
    rows
}

/// Fill one row of the checks' list.
fn fill_check(v: View<'_>, r: &CheckRow) {
    match r {
        CheckRow::Check { name, step, time, tip, action } => {
            for (part, words) in [("name", name), ("step", step), ("time", time)] {
                if let Some(p) = v.part(part) {
                    p.text(words);
                }
            }
            set_if(v, Tooltip { text: tip.as_str().into() });
            set_if(v, Action { name: action.as_str().into() });
        }
        CheckRow::Folded(words) => {
            if let Some(l) = v.part("label") {
                l.text(words);
            }
        }
        CheckRow::Why(words) => {
            v.text(words);
        }
    }
}

/// The running dot's breath (`hn.chat_status.running`'s, src/look.rs) on mark `m` while a check runs, none once none
/// does, so nothing moves on a card whose checks are done. The scene is compiled in, so a missing one is a bug.
fn breathe(m: View<'_>, on: bool) {
    use crate::look::Breath;
    let e = m.entity();
    match (on, e.has(Breath::id())) {
        (true, false) => {
            let running = view::prefab(e.world(), "hn.chat_status.running")
                .and_then(|p| p.try_cloned::<&Breath>())
                .unwrap_or_else(|| panic!("the scene's hn.chat_status.running has no Breath (scene/widgets/chat_status.flecs)"));
            m.set(running);
        }
        (false, true) => _ = e.remove(Breath::id()),
        _ => {}
    }
}

/// One line of a card.
enum Line {
    Words(String),
    /// A file's lines from where the place points, and the file (its language colours them).
    Code { text: String, file: String },
    /// A description as markdown: its blocks as the transcript's rows draw them (hn.PeekMarkdown holds them).
    Markdown(String),
    /// The title, strong, flush left.
    Title(String),
    /// The header row: an icon in an ink (a pull request's state's; None: the look's) and where the thing is.
    Site { glyph: String, ink: Option<Fill>, name: String },
    /// The header row of a file or a folder: its kind's drawing and its path.
    Art { drawing: &'static str, name: String },
    /// A folder's entries: each its row's key, its kind's drawing and its name.
    Entries(Vec<(String, &'static str, String)>),
    /// The header row of a page with a favicon (a local picture) and its site.
    Favicon { picture: String, name: String },
    /// A pull request's or issue's state: its pill, its checks' mark (breathing while one runs) and words, and the rows
    /// of its checks' list (empty while the list is shut).
    State { label: String, ink: Fill, mark: Option<(String, Fill)>, checks: String, running: bool, rows: Vec<(String, &'static str, CheckRow)> },
    Image(String),
    /// The live view of the page, by its Media key (only once a browser host made one).
    Page(u64),
}

/// A card's header row: `icon` (a glyph, in `ink` or the look's, or a file kind's drawing in its own colours) and where
/// the thing is.
fn header(icon: ChipIcon, ink: Option<Fill>, name: String) -> (&'static str, &'static str, Line) {
    match icon {
        ChipIcon::Glyph(glyph, _) => ("site", "hn.PeekSiteGlyph", Line::Site { glyph, ink, name }),
        ChipIcon::Drawing(drawing) => ("site", "hn.PeekSiteArt", Line::Art { drawing, name }),
    }
}

/// Every words node under `e` in a card's type (`body`'s size and line, each node's own role, weight and boldness kept):
/// the transcript's rows drawn smaller, on a card.
fn card_type(e: EntityView<'_>, body: TextStyle) {
    if let Some(s) = e.try_cloned::<&TextStyle>() {
        let want = TextStyle { size: body.size, line: body.line, ..s };
        if want != s {
            e.set(want);
        }
    }
    e.each_child(|c| card_type(c, body));
}

/// `v`'s `c`, set only when it differs.
fn set_if<T: harness_native_flecs::prelude::ComponentId + Clone + PartialEq>(v: View<'_>, c: T) {
    if v.entity().try_cloned::<&T>() != Some(c.clone()) {
        v.set(c);
    }
}

/// A GitHub item's card: a header of its icon in its state's ink and `owner/repo #n`, its title, its state and checks
/// (what is left of them once they are known one by one, their list while it is open), who and how much, its
/// description's start. Before an answer: asking; after a failure: the error, naming it.
fn gh_lines(w: WorldRef<'_>, tag: &Tag, r: &GhRef, it: Option<&GhItem>, at: &Checks<'_>, dark: bool, out: &mut Vec<(&'static str, &'static str, Line)>) {
    let icon = vocab::icon(w, tag, dark);
    let component = match r.kind {
        GhKind::Pr => "PR",
        GhKind::Issue => "Issue",
    };
    let fill_of = |path: &str| w.try_lookup(path).and_then(|e| e.try_cloned::<&Fill>());
    let name = format!("{} #{}", r.repo, r.n);
    let Some(it) = it.filter(|it| it.error.is_empty()) else {
        let ink = fill_of(&format!("{}::{component}::unknown", vocab::MODULE));
        out.push(header(icon, ink, name));
        let why = it.map_or_else(|| "Asking GitHub…".to_owned(), |it| it.error.clone());
        out.push(("why", "hn.PeekMeta", Line::Words(why)));
        return;
    };
    let state = it.state.map(state_name);
    let ink = state.and_then(|s| fill_of(&format!("{}::{component}::{s}", vocab::MODULE)));
    out.push(header(icon, ink, name));
    out.push(("title", "hn.PeekTitle", Line::Title(it.title.clone())));
    if let (Some(s), Some(ink)) = (it.state, ink) {
        let label = match s {
            GhState::Open => "Open",
            GhState::Draft => "Draft",
            GhState::Merged => "Merged",
            GhState::Closed => "Closed",
            GhState::NotPlanned => "Not planned",
        };
        // The card's mark is the check, the cross or the dot (the mark's `card` child); the chip's is a dot.
        let mark = it.ci.and_then(|c| {
            let e = w.try_lookup(&format!("{}::marks::{}", vocab::MODULE, ci_name(c)))?;
            let g = e.lookup_child("card")?.try_cloned::<&harness_native_ui_text_api::InlineRef>()?.glyph.as_str().to_owned();
            Some((g, e.try_cloned::<&Fill>()?))
        });
        // One by one once the daemon has them (asked only while some run or failed); until then the rollup's word.
        let known = at.known.filter(|_| matches!(it.ci, Some(GhCi::Pending | GhCi::Failure)));
        let checks = match (known.filter(|c| c.error.is_empty()), it.ci) {
            (Some(c), _) => checks::summary(&c.checks, c.total, at.now),
            (None, Some(GhCi::Success)) => "All checks passed".to_owned(),
            (None, Some(GhCi::Failure)) => "Checks failed".to_owned(),
            (None, Some(GhCi::Pending)) => "Checks running".to_owned(),
            (None, None) if r.kind == GhKind::Pr => "No checks".to_owned(),
            (None, None) => String::new(),
        };
        // The mark breathes while anything is unfinished: the frames that keep coming tick the times it shows.
        let running = known.map_or(it.ci == Some(GhCi::Pending), GhChecks::unfinished);
        let rows = known.filter(|_| at.open).map(|c| check_rows(c, at.now)).unwrap_or_default();
        out.push(("state", "hn.PeekState", Line::State { label: label.to_owned(), ink, mark, checks, running, rows }));
    }
    let mut meta = Vec::new(); // alloc: a line's parts, while it is up
    if !it.author.is_empty() {
        meta.push(it.author.clone());
    }
    if r.kind == GhKind::Pr {
        meta.push(format!("+{} \u{2212}{}", it.additions, it.deletions));
    }
    if !meta.is_empty() {
        out.push(("meta", "hn.PeekMeta", Line::Words(meta.join(" · "))));
    }
    let body = first_markdown(&it.body, if at.expand { 12 } else { 6 });
    if !body.is_empty() {
        out.push(("body", "hn.PeekMarkdown", Line::Markdown(body)));
    }
}

/// A page's card: a header of its favicon (else `globe`) and its site, its title flush left, its description, its
/// picture only when it has one, its live view only when a browser host made one; asking, or its error.
fn page_lines(url: &str, page: Option<&PageMeta>, globe: &ChipIcon, view: Option<&PageView>, out: &mut Vec<(&'static str, &'static str, Line)>) {
    let host = harness_native_ui_inline::host(url).to_owned();
    let Some(p) = page.filter(|p| p.error.is_empty()) else {
        out.push(header(globe.clone(), None, host));
        out.push(("title", "hn.PeekTitle", Line::Title(harness_native_ui_inline::bare(url).to_owned())));
        let why = page.map_or_else(|| "Fetching the page…".to_owned(), |p| p.error.clone());
        out.push(("why", "hn.PeekMeta", Line::Words(why)));
        return;
    };
    let site = if p.site.is_empty() { host } else { p.site.clone() };
    if p.icon.is_empty() {
        out.push(header(globe.clone(), None, site));
    } else {
        out.push(("site", "hn.PeekSite", Line::Favicon { picture: p.icon.clone(), name: site }));
    }
    let title = if p.title.is_empty() { harness_native_ui_inline::bare(url).to_owned() } else { p.title.clone() };
    out.push(("title", "hn.PeekTitle", Line::Title(title)));
    if !p.description.is_empty() {
        out.push(("body", "hn.PeekBody", Line::Words(p.description.clone())));
    }
    if !p.image.is_empty() {
        out.push(("image", "hn.PeekImage", Line::Image(p.image.clone())));
    }
    if let Some(v) = view.filter(|v| v.ready) {
        out.push(("page", "hn.PeekPage", Line::Page(v.key)));
    }
}

/// The first `n` lines with words of a markdown description, as markdown (its HTML comments, which GitHub's templates
/// leave, out): what a card renders as the transcript's rows.
fn first_markdown(s: &str, n: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut comment = false;
    for l in s.lines() {
        let t = l.trim();
        if comment || t.starts_with("<!--") {
            comment = !t.contains("-->");
            continue;
        }
        if t.is_empty() {
            if out.last().is_some_and(|p| !p.trim().is_empty()) {
                out.push("");
            }
            continue;
        }
        out.push(l.trim_end());
        if out.iter().filter(|l| !l.is_empty()).count() >= n {
            break;
        }
    }
    out.join("\n").trim().to_owned()
}

/// The first `n` lines with words of a description (markdown's heading marks off).
fn first_text_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(|l| l.trim().trim_start_matches('#').trim()).filter(|l| !l.is_empty() && !l.starts_with("<!--")).take(n).collect();
    lines.join("\n")
}

/// What a path's card shows of its place, read once and kept with the chat.
#[derive(Clone)]
pub(super) enum Peeked {
    /// A file's first lines, in the code face.
    Text(String),
    /// A quiet line: why the place could not be read (naming it), that a file is binary, that a folder is empty.
    Note(String),
    /// A folder's first entries (each its row's key, its kind's drawing and its name) and how many more it has.
    Folder { entries: Vec<(String, &'static str, String)>, more: usize },
}

/// What a path target (`<line>:<path>` from that line) shows: a file's first `n` lines from there, a folder's first
/// `entries` entries; what could not be read says why, naming it.
pub(super) fn peek_place(target: &str, n: usize, entries: usize) -> Peeked {
    let (from, path) = match target.split_once(':') {
        Some((l, p)) if l.parse::<usize>().is_ok() => (l.parse::<usize>().unwrap_or(1).max(1), p),
        _ => (1, target),
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Peeked::Note(format!("{path}: {e}")),
    };
    if meta.is_dir() {
        return folder(path, entries);
    }
    use std::io::Read;
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Peeked::Note(format!("{path}: {e}")),
    };
    // Bounded: FILE_PEEK_BYTES, more for a line further in (a window's worth a 200 lines, at most 64), never the whole of
    // a large file.
    let mut buf = Vec::new(); // alloc: the card's words, read once and kept with the chat
    let limit = FILE_PEEK_BYTES * (1 + from as u64 / 200).min(64);
    if let Err(e) = f.take(limit).read_to_end(&mut buf) {
        return Peeked::Note(format!("{path}: {e}"));
    }
    if buf.iter().take(4096).any(|&b| b == 0) {
        return Peeked::Note(format!("Binary, {} bytes", meta.len()));
    }
    Peeked::Text(String::from_utf8_lossy(&buf).lines().skip(from - 1).take(n).collect::<Vec<_>>().join("\n"))
}

/// A folder's entries the Finder's way: hidden ones left out, folders first (a link to a folder counts as one), then by
/// name whatever its case; the first `n`, each with its kind's drawing.
fn folder(path: &str, n: usize) -> Peeked {
    let read = match std::fs::read_dir(path) {
        Ok(d) => d,
        Err(e) => return Peeked::Note(format!("{path}: {e}")),
    };
    let mut all: Vec<(bool, String, String)> = read // alloc: the folder's names, sorted once
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let dir = e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir()));
            Some((!dir, name.to_lowercase(), name))
        })
        .collect();
    if all.is_empty() {
        return Peeked::Note("Empty folder".to_owned());
    }
    all.sort_unstable();
    let more = all.len().saturating_sub(n);
    let entries = all
        .into_iter()
        .take(n)
        .enumerate()
        .map(|(i, (file, _, name))| (format!("e{i}"), files::drawing(files::kind_at(&name, !file)), name))
        .collect();
    Peeked::Folder { entries, more }
}
