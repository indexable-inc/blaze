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
//!   [`PageView`] seam on the card for an embedded browser's live view; a path its first lines. Every card is a header
//!   row (the thing's icon, or a page's favicon, and where it is: `owner/repo #n`, the site), the title flush left, then
//!   what there is; a line with nothing to show is left out, never laid out empty.
//! - Live state: every `<PR>` and `<Issue>` in a shown chat is asked after in one `github_status` request when the chat
//!   is focused (past the daemon's cache), when its words name new ones, and every [`REASK`] seconds while it shows; the
//!   answers become `InlineLive` entities under `ui.inline.live` with `(InlineState, <state child>)` and
//!   `(InlineMark, <mark>)`, which pick the chip's icon, ink and CI mark. A failed fetch leaves the chip in its unknown
//!   state with the error, naming the repository and number, as its card's words and in the log.
//! - Links: every `<Link>` in a shown chat (a link to a pull request or issue is that component already, harness-native-
//!   ui-inline's `from_url`) has its page's card asked for once (`page_peek`, its favicon with it), at most
//!   [`PAGES_MAX`] a chat, so its chip shows the favicon at rest (`InlineLive.picture`), the globe without one.
use super::*;
use harness_native_render::inline as vocab;
use harness_native_render::markdown::{CHIP_INSET_EM, Inline};
use harness_native_render::paint::spans_of;
use harness_native_ui_inline::Tag;
use harness_native_ui_look_api::Fill;
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{Icon, InlineHot, InlineLive, InlineMark, InlineState, PageView, Picture};
use harness_proto::peek::{GhCi, GhItem, GhKind, GhRef, GhState, PageMeta};
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
    /// A path's first lines, by target, read once.
    files: BTreeMap<String, String>,
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
    pub(super) fn fill_chat_peek(&self, pv: View<'_>, st: &mut ChatState, t: &Transcript, rows: &[(String, Row)], focused: bool) {
        let w = self.world.world_ref();
        self.ask_github(w, st, t, rows, focused);
        self.sync_live(w);
        // The card first: the pointer on it is on no chip under it.
        let slot = pv.part("peek");
        let card_id = slot.and_then(|s| s.part("card")).map(View::id);
        let on_card = card_id.is_some_and(|card| {
            w.entity_named(APP)
                .target(Hovered::id(), 0)
                .is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == card))
        });
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
        self.fill_card(card, st, &c.tag);
    }

    /// Gather the chat's GitHub refs when its words change, and ask after them: fresh as the chat takes the keyboard,
    /// when there are new ones, and every [`REASK`] seconds while it shows.
    fn ask_github(&self, w: WorldRef<'_>, st: &mut ChatState, t: &Transcript, rows: &[(String, Row)], focused: bool) {
        let p = &mut st.peek;
        let mut new = false;
        // Its words move with the transcript and with what is folded (the rows shown).
        if p.rev != Some((t.rev, rows.len())) {
            p.rev = Some((t.rev, rows.len()));
            let mut refs: Vec<GhRef> = Vec::new(); // alloc: this chat's refs, kept with its state
            let mut urls: Vec<String> = Vec::new(); // alloc: this chat's links, kept with its state
            let mut live = self.peek_live.borrow_mut();
            for (_, r) in rows {
                for (_, src, md) in texts(r) {
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
            for (path, src, md) in texts(r) {
                if !md || !src.contains('<') {
                    continue;
                }
                let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
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

    /// The card's lines for `tag`: what the link knows of its GitHub item, its page, or the file it names. A link's card
    /// carries its page's [`PageView`] (the live view's seam), and its live view line only once a browser host made one.
    fn fill_card(&self, card: View<'_>, st: &mut ChatState, tag: &Tag) {
        let w = self.world.world_ref();
        let dark = self.chat_dark();
        let mut lines: Vec<(&'static str, &'static str, Line)> = Vec::new(); // alloc: a handful of lines, while it is up
        let mut page_view = None;
        if let Some((r, _)) = gh_ref(w, tag) {
            let item = self.link.peeks().gh.get(&r).cloned();
            gh_lines(w, tag, &r, item.as_ref(), dark, &mut lines);
        } else {
            // Relative paths from the session's directory, as a press opens them (chat_open_at).
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = self
                .focused_chat()
                .and_then(|(_, e, _)| w.entity_from_id(e).try_cloned::<&model::Cwd>())
                .map_or_else(|| home.clone(), |c| c.s.as_str().to_owned());
            match vocab::target(w, tag, &cwd, &home) {
                Ok(url) if url.starts_with("https://") || url.starts_with("http://") => {
                    self.link.page(&url);
                    let page = self.link.peeks().pages.get(&url).cloned();
                    let (globe, _) = vocab::icon(w, tag, dark);
                    let view = card.entity().try_cloned::<&PageView>().filter(|v| v.url.as_str() == url);
                    page_lines(&url, page.as_ref(), &globe, view.as_ref(), &mut lines);
                    page_view = Some(view.unwrap_or(PageView { url: url.as_str().into(), key: 0, ready: false }));
                }
                Ok(target) => {
                    let text = st
                        .peek
                        .files
                        .entry(target.clone())
                        .or_insert_with(|| first_lines(&target, crate::look::width(w, "inline_peek_lines") as usize));
                    let path = target.split_once(':').filter(|(n, _)| n.parse::<u32>().is_ok()).map_or(target.as_str(), |(_, p)| p);
                    let (glyph, _) = vocab::icon(w, tag, dark);
                    lines.push(("site", "hn.PeekSiteGlyph", Line::Site { glyph, ink: None, name: path.to_owned() }));
                    lines.push(("code", "hn.PeekCode", Line::Words(text.clone())));
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
        view::for_each_of(card, lines.iter().map(|(k, p, l)| (*k, *p, l)), |v, l, _| match l {
            Line::Words(s) | Line::Title(s) => {
                v.text(s);
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
            Line::Favicon { picture, name } => {
                if let Some(i) = v.part("icon") {
                    set_if(i, Picture { handle: picture.as_str().into() });
                }
                if let Some(n) = v.part("name") {
                    n.text(name);
                }
            }
            Line::State { label, ink, mark, checks } => {
                if let Some(b) = v.part("badge") {
                    set_if(b, *ink);
                    if let Some(l) = b.part("label") {
                        l.text(label);
                    }
                }
                if let Some(m) = v.part("mark") {
                    let (g, f) = mark.clone().unwrap_or_default();
                    set_if(m, Icon { glyph: g.as_str().into() });
                    set_if(m, f);
                }
                if let Some(c) = v.part("checks") {
                    c.text(checks);
                }
            }
            Line::Image(path) => set_if(v, Picture { handle: path.as_str().into() }),
            Line::Page(key) => set_if(v, harness_native_ui_text_api::Surface { key: *key, cover: 1 }),
        });
    }
}

/// One line of a card.
enum Line {
    Words(String),
    /// The title, strong, flush left.
    Title(String),
    /// The header row: an icon in an ink (a pull request's state's; None: the look's) and where the thing is.
    Site { glyph: String, ink: Option<Fill>, name: String },
    /// The header row of a page with a favicon (a local picture) and its site.
    Favicon { picture: String, name: String },
    State { label: String, ink: Fill, mark: Option<(String, Fill)>, checks: String },
    Image(String),
    /// The live view of the page, by its Media key (only once a browser host made one).
    Page(u64),
}

/// `v`'s `c`, set only when it differs.
fn set_if<T: harness_native_flecs::prelude::ComponentId + Clone + PartialEq>(v: View<'_>, c: T) {
    if v.entity().try_cloned::<&T>() != Some(c.clone()) {
        v.set(c);
    }
}

/// A GitHub item's card: a header of its icon in its state's ink and `owner/repo #n`, its title, its state and checks,
/// who and how much, its description's start. Before an answer: asking; after a failure: the error, naming it.
fn gh_lines(w: WorldRef<'_>, tag: &Tag, r: &GhRef, it: Option<&GhItem>, dark: bool, out: &mut Vec<(&'static str, &'static str, Line)>) {
    let (glyph, _) = vocab::icon(w, tag, dark);
    let component = match r.kind {
        GhKind::Pr => "PR",
        GhKind::Issue => "Issue",
    };
    let fill_of = |path: &str| w.try_lookup(path).and_then(|e| e.try_cloned::<&Fill>());
    let name = format!("{} #{}", r.repo, r.n);
    let Some(it) = it.filter(|it| it.error.is_empty()) else {
        let ink = fill_of(&format!("{}::{component}::unknown", vocab::MODULE));
        out.push(("site", "hn.PeekSiteGlyph", Line::Site { glyph, ink, name }));
        let why = it.map_or_else(|| "Asking GitHub…".to_owned(), |it| it.error.clone());
        out.push(("why", "hn.PeekMeta", Line::Words(why)));
        return;
    };
    let state = it.state.map(state_name);
    let ink = state.and_then(|s| fill_of(&format!("{}::{component}::{s}", vocab::MODULE)));
    out.push(("site", "hn.PeekSiteGlyph", Line::Site { glyph, ink, name }));
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
        let checks = match it.ci {
            Some(GhCi::Success) => "Checks passed",
            Some(GhCi::Failure) => "Checks failed",
            Some(GhCi::Pending) => "Checks running",
            None if r.kind == GhKind::Pr => "No checks",
            None => "",
        };
        out.push(("state", "hn.PeekState", Line::State { label: label.to_owned(), ink, mark, checks: checks.to_owned() }));
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
    let body = first_text_lines(&it.body, 5);
    if !body.is_empty() {
        out.push(("body", "hn.PeekBody", Line::Words(body)));
    }
}

/// A page's card: a header of its favicon (else `globe`) and its site, its title flush left, its description, its
/// picture only when it has one, its live view only when a browser host made one; asking, or its error.
fn page_lines(url: &str, page: Option<&PageMeta>, globe: &str, view: Option<&PageView>, out: &mut Vec<(&'static str, &'static str, Line)>) {
    let host = harness_native_ui_inline::host(url).to_owned();
    let Some(p) = page.filter(|p| p.error.is_empty()) else {
        out.push(("site", "hn.PeekSiteGlyph", Line::Site { glyph: globe.to_owned(), ink: None, name: host }));
        out.push(("title", "hn.PeekTitle", Line::Title(harness_native_ui_inline::bare(url).to_owned())));
        let why = page.map_or_else(|| "Fetching the page…".to_owned(), |p| p.error.clone());
        out.push(("why", "hn.PeekMeta", Line::Words(why)));
        return;
    };
    let site = if p.site.is_empty() { host } else { p.site.clone() };
    if p.icon.is_empty() {
        out.push(("site", "hn.PeekSiteGlyph", Line::Site { glyph: globe.to_owned(), ink: None, name: site }));
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

/// The first `n` lines with words of a description (markdown's heading marks off).
fn first_text_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(|l| l.trim().trim_start_matches('#').trim()).filter(|l| !l.is_empty() && !l.starts_with("<!--")).take(n).collect();
    lines.join("\n")
}

/// The first `n` lines of the file a path target names (`<line>:<path>` from that line), or of a folder its first
/// entries; what could not be read says why, naming it.
fn first_lines(target: &str, n: usize) -> String {
    let (from, path) = match target.split_once(':') {
        Some((l, p)) if l.parse::<usize>().is_ok() => (l.parse::<usize>().unwrap_or(1).max(1), p),
        _ => (1, target),
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return format!("{path}: {e}"),
    };
    if meta.is_dir() {
        let mut names: Vec<String> = match std::fs::read_dir(path) {
            Ok(d) => d.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect(),
            Err(e) => return format!("{path}: {e}"),
        };
        names.sort();
        return names.into_iter().take(n).collect::<Vec<_>>().join("\n");
    }
    use std::io::Read;
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return format!("{path}: {e}"),
    };
    // Bounded: FILE_PEEK_BYTES, more for a line further in (a window's worth a 200 lines, at most 64), never the whole of
    // a large file.
    let mut buf = Vec::new(); // alloc: the card's words, read once and kept with the chat
    let limit = FILE_PEEK_BYTES * (1 + from as u64 / 200).min(64);
    if let Err(e) = f.take(limit).read_to_end(&mut buf) {
        return format!("{path}: {e}");
    }
    if buf.iter().take(4096).any(|&b| b == 0) {
        return format!("{path}: binary, {} bytes", meta.len());
    }
    String::from_utf8_lossy(&buf).lines().skip(from - 1).take(n).collect::<Vec<_>>().join("\n")
}
