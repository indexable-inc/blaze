//! Den's model and effort picker in a chat's composer bar (scene/widgets/chat.flecs `hn.ChatPick`), and its context meter.
//!
//! The bar's leading edge shows one quiet control, "Opus · Medium" and a small chevron, on a wash under the pointer, its
//! words easing toward the ink. A press opens the menu above it: the models and the efforts (Command-/ the models alone,
//! Shift-Command-/ with nothing to step to the efforts alone), the current rows ticked. A pick sends `Request::SetModel` (link.rs `chat_choose`);
//! the daemon has the agent switch (claude's `set_model` and `apply_flag_settings` control requests, codex's next
//! `turn/start`) and logs the choice, so a resumed chat keeps it. A chat whose own log has no `models` event (one from
//! before chats listed them, or one whose agent is not running) picks from the list its agent kind last gave the daemon
//! (`list`'s `models`, link.rs `agent_models`); an ended chat's pick is recorded and its resume starts with it. Until any
//! agent of its kind has listed, or for a model with no efforts, that part of the menu says why in a short line. A press
//! anywhere else, or Escape, closes it.
//!
//! The menu (unless it is the efforts alone) is a searchable menu (search_menu.flecs `ui.SearchMenu`,
//! harness_native_ui_view::search): typing while it is open filters it, the first match highlighted, Return picks. It
//! offers every agent kind's models, the chat's own first, under each kind's head with its maker's mark. One of the
//! chat's own switches it as above; another kind's cannot (a chat's agent is one process for its life: Claude's
//! stream-json or Codex's app-server, each with its own conversation), so it starts a chat of that kind on that model in
//! the same directory (`chat:spawn:`), which replaces this one when nothing was sent in it yet. Every pick is remembered
//! ([`App::remember_choice`]): cmd+N starts on the last agent, model and effort.
//!
//! At the bar's trailing edge, before the send button, the context meter: a small ring filling as the conversation fills
//! its model's window (den's: muted, the warning tone from [`WARN`], the failed tone from [`FULL`]); under the pointer,
//! its popover (context.rs) says how full, with the percentage, and with what.
use super::*;
use harness_native_ui_look_api::Stroke;
use harness_native_ui_text_api::Vector;
use harness_proto::event::ModelChoice;
use harness_native_ui_view::search::{self, Edit, Entry};
use std::collections::BTreeMap;

/// The share of the window from which the meter shows in the warning tone, and from which in the failed tone (den's).
const WARN: f64 = 0.8;
const FULL: f64 = 0.95;
/// The track under the ring: the ink at this alpha (den's).
const TRACK_ALPHA: f32 = 0.18;

/// Which of the picker's menus is open: the models, the efforts, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Model,
    Effort,
    All,
}

impl Pick {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model" => Some(Self::Model),
            "effort" => Some(Self::Effort),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The models chat `t` (of agent kind `kind`) picks from: those its own log listed, else those its kind last listed to
/// the daemon (`kept`).
fn models_of<'a>(t: &'a Transcript, kind: &str, kept: &'a BTreeMap<String, Vec<ModelChoice>>) -> &'a [ModelChoice] {
    if !t.models.is_empty() {
        return &t.models;
    }
    let kind = if t.agent.is_empty() { kind } else { t.agent.as_str() };
    kept.get(kind).map_or(&[], Vec::as_slice)
}

/// Whose mark a model's row in the menu shows, as an `hn.Kind` child: Claude's for Anthropic's models, OpenAI's
/// (`codex`) for GPT and the o-series, else the agent kind's own (`agent`: a Cursor chat's `auto`), from the model's id
/// and the model it runs as (`cur`), else `model`.
fn maker(cur: Option<&ModelChoice>, model: &str, agent: &str) -> &'static str {
    let names = cur.map_or([model, ""], |c| [c.id.as_str(), c.model.as_str()]);
    let is = |pre: &[&str]| names.iter().any(|n| pre.iter().any(|p| n.to_ascii_lowercase().starts_with(p)));
    if is(&["claude", "opus", "sonnet", "haiku", "fable"]) {
        "claude"
    } else if is(&["gpt", "o1", "o3", "o4", "codex"]) {
        "codex"
    } else {
        match agent {
            "claude" => "claude",
            "codex" => "codex",
            "cursor" => "cursor",
            _ => "other",
        }
    }
}

/// The model the chat runs, among `models`: what it runs as, else what was last chosen.
fn current<'a>(t: &Transcript, models: &'a [ModelChoice]) -> Option<&'a ModelChoice> {
    let by = |m: &str| if m.is_empty() { None } else { models.iter().find(|c| c.is(m)) };
    by(&t.model).or_else(|| by(&t.chosen_model))
}

/// An effort's word as den shows it: `xhigh` is Extra High, the rest capitalised.
fn effort_word(e: &str) -> String {
    if e == "xhigh" {
        return "Extra High".to_owned();
    }
    let mut c = e.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A token count as the meter's popover says it: 950, 46.2k, 258k, 1M.
pub(super) fn tokens(n: u64) -> String {
    let (v, unit) = match n {
        1_000_000.. => (n as f64 / 1e6, "M"),
        1_000.. => (n as f64 / 1e3, "k"),
        _ => return n.to_string(),
    };
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// The ring's arc for share `f` of the window: clockwise from the top of a 12-unit box, radius 5 (a hair at the least, the
/// whole circle at the most).
fn arc(f: f64) -> String {
    let f = f.clamp(0.01, 1.0);
    if f >= 0.999 {
        return "M6 1 A5 5 0 1 1 6 11 A5 5 0 1 1 6 1".to_owned();
    }
    let a = std::f64::consts::TAU * f;
    let (x, y) = (6.0 + 5.0 * a.sin(), 6.0 - 5.0 * a.cos());
    format!("M6 1 A5 5 0 {} 1 {x:.3} {y:.3}", u8::from(f > 0.5))
}

fn mix(a: Rgba, b: Rgba, k: f32) -> Rgba {
    let m = |x: f32, y: f32| x + (y - x) * k;
    Rgba { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: m(a.a, b.a) }
}

/// The Fill of scene prefab `path` (`hn.ChatPickInk.rest`); the scene always has it.
fn fill_of(w: WorldRef<'_>, path: &str) -> Fill {
    view::prefab(w, path).and_then(|p| p.try_cloned::<&Fill>()).unwrap_or_else(|| panic!("scene/widgets/chat.flecs has no Fill on {path}"))
}

fn set_fill(v: View<'_>, f: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(f) {
        v.set(f);
    }
}

impl App {
    /// The composer bar's picker and meter for chat `sid` (transcript `t`, an agent of kind `kind`, running `model` at
    /// `effort` as the pane has them), and its menu while `open`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fill_chat_pick(
        &self, bar: View<'_>, sid: u64, t: &Transcript, kind: &str, open: Option<Pick>, nav: &mut PickNav, model: &str,
        effort: &str,
    ) {
        let w = self.world.world_ref();
        let kept = self.link.agent_models();
        let models = models_of(t, kind, &kept);
        let cur = current(t, models);
        let name = cur.map(|c| if c.name.is_empty() { c.id.as_str() } else { c.name.as_str() }).unwrap_or(model);
        let context = t.context.as_ref().filter(|c| c.window > 0);
        let (Some(pick), Some(meter_slot)) = (bar.part("pick"), bar.part("meter")) else { return };
        let (rest, hot) = (fill_of(w, "hn.ChatPickInk.rest"), fill_of(w, "hn.ChatPickInk.hot"));
        // "Opus · Medium": the model, and its effort when known.
        let words = if effort.is_empty() { name.to_owned() } else { format!("{name} \u{b7} {}", effort_word(effort)) };
        let shown = (!name.is_empty()).then_some(("model", "hn.ChatPick", ()));
        view::for_each_of(pick, shown, |v, (), _| control(v, &format!("chat:pick:{sid}:all"), &words, rest, hot));
        view::for_each_of(meter_slot, context.map(|_| ("context", "hn.ChatContext", ())), |v, (), _| {
            if let Some(c) = context {
                meter(v, (c.used, c.window), hot);
            }
        });
        // Under the pointer, the meter's popover (context.rs), anchored to the ring; never over the open menu.
        if let Some(pop) = bar.part("pop") {
            let agent = if t.agent.is_empty() { kind } else { t.agent.as_str() };
            super::context::fill_context_pop(pop, meter_slot.part("context"), context, agent, open.is_some());
        }
        // Open, the menu floats above the control, its leading edge on the control's.
        let trigger = open.and_then(|_| pick.part("model"));
        let Some(menu) = bar.part("menu") else { return };
        // The efforts alone are a plain menu; the models (and both) a searchable one.
        let plate = match open {
            Some(Pick::Effort) => Some(("plate", "ui.Menu")),
            Some(_) => Some(("search", "ui.SearchMenu")),
            None => None,
        };
        view::for_each_of(menu, plate.filter(|_| trigger.is_some()).map(|(k, p)| (k, p, ())), |_, (), _| {});
        let plate = plate.and_then(|(k, _)| menu.part(k));
        if let (Some(plate), Some(trigger), Some(page)) = (plate, trigger, open) {
            plate.anchored(trigger.id());
            let kind = if t.agent.is_empty() { kind } else { t.agent.as_str() };
            let hovered = w.entity_named(APP).target(Hovered::id(), 0);
            let titles = self.agent_titles();
            let ctx = MenuCtx { sid, own: models, kind, kept: &kept, cur, name, effort, titles: &titles };
            menu_rows(w, plate, &ctx, page, hovered, nav);
        }
    }

    /// Open chat `sid`'s menu `page`, or close it when that one is open (a press on its control again); any other chat's
    /// closes.
    pub(in crate::app) fn chat_pick_toggle(&mut self, sid: u64, page: Pick) {
        let mut states = self.chat.borrow_mut();
        for (s, st) in states.iter_mut() {
            if *s != sid {
                st.pick = None;
            }
        }
        let st = states.entry(sid).or_default();
        st.pick = if st.pick == Some(page) { None } else { Some(page) };
        st.pick_nav = PickNav::default();
        drop(states);
        self.poke();
    }

    /// An action from the picker: `<sid>:<page>` opens or closes a menu (`chat:pick:`).
    pub(in crate::app) fn chat_pick_action(&mut self, rest: &str) {
        let Some((sid, page)) = rest.split_once(':') else { return };
        if let (Ok(sid), Some(page)) = (sid.parse(), Pick::parse(page)) {
            self.chat_pick_toggle(sid, page);
        }
    }

    /// A pick from the menu (`chat:set:<sid>:model:<id>` or `...:effort:<level>`): sent to the daemon, the menu closed.
    pub(in crate::app) fn chat_pick_set(&mut self, rest: &str) {
        let Some((sid, rest)) = rest.split_once(':') else { return };
        let Ok(sid) = sid.parse::<u64>() else { return };
        let (model, effort) = match rest.split_once(':') {
            Some(("model", m)) => (Some(m.to_owned()), None),
            Some(("effort", e)) => (None, Some(e.to_owned())),
            _ => return,
        };
        self.link.chat_choose(sid, model.clone(), effort.clone());
        self.remember_chat_choice(sid, model, effort);
        if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
            st.pick = None;
        }
        self.poke();
    }

    /// A pick of another agent kind's model (`chat:spawn:<sid>:<kind>:<model>`): a chat of that kind on that model in
    /// chat `sid`'s directory, selected once it exists; chat `sid` is archived in its place when nothing was sent in it.
    pub(in crate::app) fn chat_pick_spawn(&mut self, rest: &str) {
        let mut it = rest.splitn(3, ':');
        let (Some(sid), Some(kind), Some(m)) = (it.next().and_then(|s| s.parse::<u64>().ok()), it.next(), it.next()) else { return };
        let (kind, m) = (kind.to_owned(), m.to_owned());
        self.chat_pick_close();
        let e = self.entity_of(sid);
        let cwd = e
            .and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Cwd>())
            .map(|c| c.s.as_str().to_owned())
            .filter(|c| !c.is_empty());
        let fresh = self.chats.borrow().get(&sid).is_none_or(|t| !t.items.iter().any(|i| matches!(i, model::chat::Item::User { .. })));
        self.remember_choice(&kind, Some(&m), None);
        self.spawning(&kind);
        self.spawn_on(&kind, Some(m), None, cwd);
        if fresh && let Some(e) = e {
            self.link.kill(sid);
            self.archive_object(e);
        }
    }

    /// Chat `sid` now runs `model` (or keeps its own) at `effort` (or keeps its own), as the person chose: remembered
    /// for cmd+N under its agent kind. A new model without an effort starts at its own default.
    fn remember_chat_choice(&mut self, sid: u64, model: Option<String>, effort: Option<String>) {
        let (kind, model, effort) = {
            let chats = self.chats.borrow();
            let Some(t) = chats.get(&sid) else { return };
            let kept = self.link.agent_models();
            let kind = match self.entity_of(sid) {
                Some(e) if t.agent.is_empty() => model::kind_name(self.world.world_ref().entity_from_id(e)).to_owned(),
                _ => t.agent.clone(),
            };
            let cur = current(t, models_of(t, &kind, &kept)).map(|c| c.id.clone());
            let same = model.is_none() || model == cur;
            let effort = effort.or_else(|| Some(t.effort.clone()).filter(|e| same && !e.is_empty()));
            (kind, model.or(cur), effort)
        };
        self.remember_choice(&kind, model.as_deref(), effort.as_deref());
    }

    /// Each agent kind's name, from the act.Spawn actions that have a title (scene/20-keys.flecs `SpawnKind`).
    pub(in crate::app) fn agent_titles(&self) -> BTreeMap<String, String> {
        self.spawns().into_iter().filter(|p| !p.title.is_empty()).map(|p| (p.kind, p.title)).collect()
    }

    /// Type into chat `sid`'s open searchable menu (the models', or both): whether it took the text.
    pub(in crate::app) fn chat_pick_type(&mut self, sid: u64, e: Edit<'_>) -> bool {
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid).filter(|st| matches!(st.pick, Some(Pick::Model | Pick::All))) else { return false };
        st.pick_nav.search.edit(e);
        drop(states);
        self.poke();
        true
    }

    /// Open chat `sid`'s models with `why` at their top (cmd+N's remembered model is no longer offered).
    pub(in crate::app) fn chat_pick_refuse(&mut self, sid: u64, why: String) {
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        st.pick = Some(Pick::Model);
        st.pick_nav = PickNav { error: Some(why), ..PickNav::default() };
        drop(states);
        self.poke();
    }

    /// Close every open picker menu. Whether one was open (Escape takes it then, before it would interrupt).
    pub(in crate::app) fn chat_pick_close(&mut self) -> bool {
        let mut any = false;
        for st in self.chat.borrow_mut().values_mut() {
            any |= st.pick.take().is_some();
        }
        if any {
            self.poke();
        }
        any
    }

    /// A press off an open menu and off the picker closes it (before this frame's actions run, so a press on a control
    /// is that control's).
    pub(in crate::app) fn step_chat_pick(&mut self) {
        if !self.pointer.went_down || self.chat.borrow().values().all(|st| st.pick.is_none()) {
            return;
        }
        let w = self.world.world_ref();
        let menus = [view::prefab(w, "ui.Menu"), view::prefab(w, "ui.SearchMenu")];
        let ours = |n: EntityView<'_>| {
            n.try_cloned::<&Action>().is_some_and(|a| ["chat:pick:", "chat:set:", "chat:spawn:"].iter().any(|p| a.name.as_str().starts_with(p)))
                || menus.iter().flatten().any(|m| n.owns((flecs::IsA::ID, *m.id())))
        };
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        let inside = hovered.is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(ours));
        if !inside {
            self.chat_pick_close();
        }
    }

    /// Command-/ (`pick_model`): the focused chat's models, open or closed. Shift-Command-/ (`effort_next`): its effort
    /// steps to the next its model takes (round to the first); a model with none opens the efforts' menu, which says so.
    pub(in crate::app) fn chat_pick_key(&mut self, op: &str) {
        let Some((_, e, sid)) = self.focused_chat() else { return };
        if op == "pick_model" {
            return self.chat_pick_toggle(sid, Pick::Model);
        }
        let next = {
            let chats = self.chats.borrow();
            let kept = self.link.agent_models();
            let kind = model::kind_name(self.world.world_ref().entity_from_id(e));
            chats.get(&sid).and_then(|t| {
                let efforts = &current(t, models_of(t, kind, &kept))?.efforts;
                let at = efforts.iter().position(|e| *e == t.effort).map_or(0, |i| (i + 1) % efforts.len().max(1));
                efforts.get(at).cloned()
            })
        };
        match next {
            Some(e) => {
                self.link.chat_choose(sid, None, Some(e.clone()));
                self.remember_chat_choice(sid, None, Some(e));
                self.poke();
            }
            None => self.chat_pick_toggle(sid, Pick::Effort),
        }
    }

    /// A key while chat `sid`'s menu is open: the arrows move its highlight, Return picks the highlighted row (or closes
    /// the menu when none is). Whether the menu took the key (it is open and the key is one of those).
    pub(in crate::app) fn chat_pick_nav(&mut self, sid: u64, op: &str) -> bool {
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid).filter(|st| st.pick.is_some()) else { return false };
        let searching = st.pick != Some(Pick::Effort);
        let n = &mut st.pick_nav;
        let edit = match op {
            "backspace" => Some(Edit::Erase),
            "word_erase" => Some(Edit::EraseWord),
            "line_erase" => Some(Edit::Clear),
            _ => None,
        };
        if let Some(e) = edit {
            if searching {
                n.search.edit(e);
            }
            drop(states);
            self.poke();
            return true;
        }
        match op {
            "up" | "down" => {
                let picks = &n.picks;
                n.search.nav.step(op == "down", picks.len(), |i| picks[i].is_some());
                drop(states);
                self.poke();
                true
            }
            "send" | "submit" => {
                let pick = n.search.nav.hot.and_then(|i| n.picks.get(i).cloned().flatten());
                drop(states);
                match pick.as_deref() {
                    Some(a) if a.starts_with("chat:set:") => self.chat_pick_set(&a["chat:set:".len()..]),
                    Some(a) if a.starts_with("chat:spawn:") => self.chat_pick_spawn(&a["chat:spawn:".len()..]),
                    _ => {
                        self.chat_pick_close();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// For an offscreen shot (shot.rs `HARNESS_NATIVE_SHOT_CHAT_MENU`): chat `sid`'s menu `page` open.
    /// `page` may carry a query typed into it after a colon (`model:son`).
    pub fn shot_chat_pick(&mut self, sid: u64, page: &str) {
        let (page, query) = page.split_once(':').unwrap_or((page, ""));
        if let Some(p) = Pick::parse(page) {
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.pick = Some(p);
            st.pick_nav.search.edit(Edit::Type(query));
        }
    }
}

/// The picker's control: its action, its words in `rest` eased toward `hot` by its hover spring.
fn control(v: View<'_>, action: &str, words: &str, rest: Fill, hot: Fill) {
    v.action(action);
    let e = v.entity();
    if !e.has(HoverAmt::id()) {
        e.set(HoverAmt::default());
    }
    let k = e.try_cloned::<&HoverAmt>().map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
    let Some(p) = v.part("label") else { return };
    p.text(words);
    set_fill(p, Fill { dark: mix(rest.dark, hot.dark, k), light: mix(rest.light, hot.light, k) });
}

/// The context meter for `(used, window)` tokens: the ring's arc in its tone, the track in `ink` faintly. The number is
/// the popover's (context.rs).
fn meter(v: View<'_>, (used, window): (u64, u64), ink: Fill) {
    let w = v.entity().world();
    let f = used as f64 / window as f64;
    let tone = fill_of(w, if f >= FULL { "hn.ChatContextTone.full" } else if f >= WARN { "hn.ChatContextTone.warn" } else { "hn.ChatContextTone.ok" });
    let Some(ring) = v.part("ring") else { return };
    if let Some(track) = ring.part("track") {
        let s = Stroke { dark: Rgba { a: TRACK_ALPHA, ..ink.dark }, light: Rgba { a: TRACK_ALPHA, ..ink.light } };
        if track.entity().try_cloned::<&Stroke>() != Some(s) {
            track.set(s);
        }
    }
    if let Some(a) = ring.part("arc") {
        let s = Stroke { dark: tone.dark, light: tone.light };
        if a.entity().try_cloned::<&Stroke>() != Some(s) {
            a.set(s);
        }
        let d = arc(f);
        if let Some(mut vec) = a.entity().try_cloned::<&Vector>()
            && vec.d.as_str() != d
        {
            vec.d = d.as_str().into();
            a.set(vec);
        }
    }
}

/// The open menu's query and highlight (the pointer's and the arrow keys', harness_native_ui_view::search), what each of
/// its rows picks (None for a head, a separator or a note) as last drawn (what Return picks), and an error to show at
/// its top.
#[derive(Default)]
pub struct PickNav {
    search: search::Search,
    picks: Vec<Option<String>>,
    error: Option<String>,
}

/// A choice in the picker's menu.
struct Choice {
    label: String,
    about: String,
    on: bool,
    action: String,
    /// Its maker's mark (an `hn.Kind` child); empty for an effort.
    mark: &'static str,
}

/// What the menu is built from: chat `sid`'s own models (`own`, of agent kind `kind`), every kind's kept lists, the
/// model it runs (`cur`, named `name`) and its `effort`.
struct MenuCtx<'a> {
    sid: u64,
    own: &'a [ModelChoice],
    kind: &'a str,
    kept: &'a BTreeMap<String, Vec<ModelChoice>>,
    cur: Option<&'a ModelChoice>,
    name: &'a str,
    effort: &'a str,
    /// Each agent kind's name ([`App::agent_titles`]).
    titles: &'a BTreeMap<String, String>,
}

/// Agent kind `kind`'s name, as a head and in a note: its act.Spawn's `SpawnKind.title` (scene/20-keys.flecs, in
/// `titles`), else the kind itself.
fn kind_title<'a>(titles: &'a BTreeMap<String, String>, kind: &'a str) -> &'a str {
    titles.get(kind).map_or(kind, String::as_str)
}

/// The open menu's rows on its plate (the efforts' `ui.Menu`, else a `ui.SearchMenu`'s `items` under its filter): the
/// models of every kind under their heads, the chat's own first (`page` Model), the current model's efforts (Effort), or
/// both (All), filtered by what is typed; what cannot be chosen says why instead. The highlight goes where the pointer
/// (on `hovered`) or the keys put it (`nav`).
fn menu_rows(w: WorldRef<'_>, plate: View<'_>, c: &MenuCtx<'_>, page: Pick, hovered: Option<EntityView<'_>>, nav: &mut PickNav) {
    let mut rows: Vec<Entry<Choice>> = Vec::with_capacity(c.own.len() + 24); // alloc: the open menu's rows, once a frame
    if page != Pick::Effort {
        let others = c.kept.iter().filter(|(k, _)| k.as_str() != c.kind).map(|(k, m)| (k.as_str(), m.as_slice()));
        for (kind, models) in std::iter::once((c.kind, c.own)).chain(others) {
            if !rows.is_empty() {
                rows.push(Entry::Sep);
            }
            rows.push(Entry::Head(kind_title(c.titles, kind).to_owned()));
            if models.is_empty() {
                rows.push(Entry::Note(format!("Model list arrives when a {} chat starts.", kind_title(c.titles, kind))));
                continue;
            }
            let own = kind == c.kind;
            rows.extend(models.iter().map(|m| {
                let label = if m.name.is_empty() { m.id.clone() } else { m.name.clone() };
                Entry::Item {
                    text: format!("{label} {} {} {}", m.id, m.about, kind_title(c.titles, kind)),
                    data: Choice {
                        label,
                        about: m.about.clone(),
                        on: own && c.cur.is_some_and(|x| x.id == m.id),
                        action: if own {
                            format!("chat:set:{}:model:{}", c.sid, m.id)
                        } else {
                            format!("chat:spawn:{}:{kind}:{}", c.sid, m.id)
                        },
                        mark: maker(Some(m), "", kind),
                    },
                }
            }));
        }
    }
    if page == Pick::All {
        rows.push(Entry::Sep);
        rows.push(Entry::Head("Effort".to_owned()));
    }
    if page != Pick::Model {
        match c.cur {
            Some(m) if !m.efforts.is_empty() => rows.extend(m.efforts.iter().map(|e| Entry::Item {
                text: format!("{} effort", effort_word(e)),
                data: Choice {
                    label: effort_word(e),
                    about: String::new(),
                    on: e == c.effort,
                    action: format!("chat:set:{}:effort:{e}", c.sid),
                    mark: "",
                },
            })),
            Some(_) => rows.push(Entry::Note(format!("{} has no effort levels.", c.name))),
            None if c.own.is_empty() => rows.push(Entry::Note(format!("Model list arrives when a {} chat starts.", kind_title(c.titles, c.kind)))),
            None => rows.push(Entry::Note(format!("{} is not in the model list, so its efforts are unknown.", c.name))),
        }
    }
    let searching = page != Pick::Effort;
    let rows = if searching { search::filter(&nav.search.query, rows) } else { rows };
    // An error leads (cmd+N's remembered model no longer offered), until something is typed.
    let rows: Vec<Entry<Choice>> = match nav.error.as_ref().filter(|_| nav.search.query.is_empty()) {
        Some(e) => std::iter::once(Entry::Note(e.clone())).chain(rows).collect(),
        None => rows,
    };
    nav.picks.clear();
    nav.picks.extend(rows.iter().map(|r| match r {
        Entry::Item { data, .. } => Some(data.action.clone()),
        _ => None,
    }));
    let picks = &nav.picks;
    nav.search.first(picks.len(), |i| picks[i].is_some());
    let items = if searching {
        search::show(plate, &nav.search.query, "Search models");
        plate.part("items").unwrap_or(plate)
    } else {
        plate
    };
    // The row the pointer is on (one of the items' children, or inside one), if it can be chosen.
    let items_id = items.id();
    let under = hovered
        .and_then(|h| std::iter::successors(Some(h), |n| n.parent()).take(16).find(|n| n.parent().is_some_and(|p| p.id() == items_id)))
        .and_then(|row| row.name().strip_prefix('r').and_then(|i| i.parse::<usize>().ok()))
        .filter(|&i| nav.picks.get(i).is_some_and(Option::is_some));
    nav.search.nav.pointer(under);
    let hot = nav.search.nav.hot;
    let keyed = rows.iter().enumerate().map(|(i, r)| {
        let prefab = match r {
            Entry::Head(_) => "ui.MenuHead",
            Entry::Sep => "ui.MenuSep",
            Entry::Item { data: Choice { on: true, mark: "", .. }, .. } => "ui.MenuItemOn",
            Entry::Item { data: Choice { mark: "", .. }, .. } => "ui.MenuItem",
            Entry::Item { data: Choice { on: true, .. }, .. } => "ui.MenuItemMarkedOn",
            Entry::Item { .. } => "ui.MenuItemMarked",
            Entry::Note(_) => "ui.MenuNote",
        };
        (format!("r{i}"), prefab, (i, r))
    });
    view::for_each_of(items, keyed, |v, (i, r), _| match r {
        Entry::Head(s) | Entry::Note(s) => {
            if let Some(l) = v.part("label") {
                l.text(s);
            }
        }
        Entry::Item { data: c, .. } => {
            harness_native_ui_view::menu::light(v.entity(), hot == Some(i));
            v.action(&c.action);
            if let Some(l) = v.part("label") {
                l.text(&c.label);
            }
            if let Some(a) = v.part("about") {
                a.text(&c.about);
            }
            if !c.mark.is_empty()
                && let Some(icon) = v.part("lead").and_then(|l| l.part("icon"))
            {
                crate::look::kind_mark(w, icon.entity(), c.mark);
            }
        }
        Entry::Sep => {}
    });
}
