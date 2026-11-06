//! Den's model and effort picker under a chat's composer (scene/widgets/chat.flecs `hn.ChatPick*`), and its context meter.
//!
//! The status line shows the model's name, the effort's word and a chevron, each its own control with no plate whose ink
//! eases toward the strong ink under the pointer. A press opens the menu above it: the models (the name), the efforts
//! (the word) or both (the chevron), the current row ticked. A pick sends `Request::SetModel` (link.rs `chat_choose`);
//! the daemon has the agent switch (claude's `set_model` and `apply_flag_settings` control requests, codex's next
//! `turn/start`) and logs the choice, so a resumed chat keeps it. A chat whose own log has no `models` event (one from
//! before chats listed them, or one whose agent is not running) picks from the list its agent kind last gave the daemon
//! (`list`'s `models`, link.rs `agent_models`); an ended chat's pick is recorded and its resume starts with it. Until any
//! agent of its kind has listed, or for a model with no efforts, that part of the menu says why in a short line. A press
//! anywhere else, or Escape, closes it.
//!
//! After them, the context meter: a ring filling as the conversation fills its model's window, and the percentage (den's:
//! muted, the warning tone from [`WARN`], the failed tone from [`FULL`]), its tooltip the tokens.
use super::*;
use harness_native_ui_look_api::Stroke;
use harness_native_ui_text_api::Vector;
use harness_proto::event::ModelChoice;
use harness_native_ui_view::menu;
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

    /// The control that opens it (a part of the status line's `pick`).
    fn trigger(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort",
            Self::All => "more",
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

/// Whose mark the model's control shows, as an `hn.Kind` child: Claude's for Anthropic's models, OpenAI's (`codex`) for
/// GPT and the o-series, else the agent kind's own (`agent`: a Cursor chat's `auto`), from the model's id and the model
/// it runs as (`cur`), else the status line's `model`.
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

/// A token count as den's tooltip says it: 950, 46.2k, 258k, 1M.
fn tokens(n: u64) -> String {
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

/// The Fill of scene prefab `path` (`hn.ChatPickInk.word`); the scene always has it.
fn fill_of(w: WorldRef<'_>, path: &str) -> Fill {
    view::prefab(w, path).and_then(|p| p.try_cloned::<&Fill>()).unwrap_or_else(|| panic!("scene/widgets/chat.flecs has no Fill on {path}"))
}

fn set_fill(v: View<'_>, f: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(f) {
        v.set(f);
    }
}

impl App {
    /// The status line's picker and meter for chat `sid` (transcript `t`, an agent of kind `kind`, running `model` at
    /// `effort` as the status line has them), and its menu while `open`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fill_chat_pick(
        &self, status: View<'_>, sid: u64, t: &Transcript, kind: &str, open: Option<Pick>, nav: &mut PickNav, model: &str,
        effort: &str,
    ) {
        let w = self.world.world_ref();
        let kept = self.link.agent_models();
        let models = models_of(t, kind, &kept);
        let cur = current(t, models);
        let name = cur.map(|c| if c.name.is_empty() { c.id.as_str() } else { c.name.as_str() }).unwrap_or(model);
        let context = t.context.filter(|&(_, window)| window > 0);
        let Some(pick) = status.part("pick") else { return };
        let mut parts: Vec<(&str, &str)> = Vec::with_capacity(4); // alloc: four parts at most, once a frame per chat
        if !name.is_empty() {
            parts.push(("model", "hn.ChatPick"));
            if !effort.is_empty() {
                parts.push(("effort", "hn.ChatPickWord"));
            }
            parts.push(("more", "hn.ChatPickMore"));
        }
        if context.is_some() {
            parts.push(("context", "hn.ChatContext"));
        }
        let (ink, word) = (fill_of(w, "hn.ChatPickInk.name"), fill_of(w, "hn.ChatPickInk.word"));
        // Under the pointer, each eases to the name's ink at full strength.
        let hot = Fill { dark: Rgba { a: 1.0, ..ink.dark }, light: Rgba { a: 1.0, ..ink.light } };
        let effort_shown = effort_word(effort);
        view::for_each_of(pick, parts.iter().map(|&(k, p)| (k, p, k)), |v, k, _| match k {
            "model" => {
                control(v, &format!("chat:pick:{sid}:model"), "label", Some(name), ink, hot);
                if let Some(i) = v.part("icon") {
                    crate::look::kind_mark(w, i.entity(), maker(cur, model, if t.agent.is_empty() { kind } else { &t.agent }));
                }
            }
            "effort" => control(v, &format!("chat:pick:{sid}:effort"), "label", Some(&effort_shown), word, hot),
            "more" => control(v, &format!("chat:pick:{sid}:all"), "icon", None, word, hot),
            _ => {
                if let Some(c) = context {
                    meter(v, c, ink);
                }
            }
        });
        // Open, the menu floats above what opened it, its leading edge on that control's (the whole picker for the
        // chevron's menu of both, so it lines up with the model's name; the chevron when the effort's word is not there).
        let trigger = open.and_then(|p| if p == Pick::All { Some(pick) } else { pick.part(p.trigger()).or_else(|| pick.part("more")) });
        let Some(menu) = status.part("menu") else { return };
        view::for_each_of(menu, trigger.map(|_| ("plate", "ui.Menu", ())), |_, (), _| {});
        if let (Some(plate), Some(trigger), Some(page)) = (menu.part("plate"), trigger, open) {
            plate.anchored(trigger.id());
            let kind = if t.agent.is_empty() { kind } else { t.agent.as_str() };
            let hovered = w.entity_named(APP).target(Hovered::id(), 0);
            menu_rows(plate, sid, models, kind, cur, page, name, effort, hovered, nav);
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
        match rest.split_once(':') {
            Some(("model", m)) => self.link.chat_choose(sid, Some(m.to_owned()), None),
            Some(("effort", e)) => self.link.chat_choose(sid, None, Some(e.to_owned())),
            _ => return,
        }
        if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
            st.pick = None;
        }
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
        let menu = view::prefab(w, "ui.Menu");
        let ours = |n: EntityView<'_>| {
            n.try_cloned::<&Action>().is_some_and(|a| a.name.as_str().starts_with("chat:pick:") || a.name.as_str().starts_with("chat:set:"))
                || menu.is_some_and(|m| n.owns((flecs::IsA::ID, *m.id())))
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
                self.link.chat_choose(sid, None, Some(e));
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
        let n = &mut st.pick_nav;
        match op {
            "up" | "down" => {
                let picks = &n.picks;
                n.nav.step(op == "down", picks.len(), |i| picks[i].is_some());
                drop(states);
                self.poke();
                true
            }
            "send" | "submit" => {
                let pick = n.nav.hot.and_then(|i| n.picks.get(i).cloned().flatten());
                drop(states);
                match pick.as_deref().and_then(|a| a.strip_prefix("chat:set:")) {
                    Some(rest) => self.chat_pick_set(rest),
                    None => {
                        self.chat_pick_close();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// For an offscreen shot (shot.rs `HARNESS_NATIVE_SHOT_CHAT_MENU`): chat `sid`'s menu `page` open.
    pub fn shot_chat_pick(&mut self, sid: u64, page: &str) {
        if let Some(p) = Pick::parse(page) {
            self.chat.borrow_mut().entry(sid).or_default().pick = Some(p);
        }
    }
}

/// One of the picker's controls: its action, its words (or its glyph) in `rest` eased toward `hot` by its hover spring.
fn control(v: View<'_>, action: &str, part: &str, words: Option<&str>, rest: Fill, hot: Fill) {
    v.action(action);
    let e = v.entity();
    if !e.has(HoverAmt::id()) {
        e.set(HoverAmt::default());
    }
    let k = e.try_cloned::<&HoverAmt>().map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
    let Some(p) = v.part(part) else { return };
    if let Some(s) = words {
        p.text(s);
    }
    set_fill(p, Fill { dark: mix(rest.dark, hot.dark, k), light: mix(rest.light, hot.light, k) });
}

/// The context meter for `(used, window)` tokens: the ring's arc and the percentage in their tone, the track in `ink`
/// faintly, the tokens in its tooltip.
fn meter(v: View<'_>, (used, window): (u64, u64), ink: Fill) {
    let w = v.entity().world();
    let f = used as f64 / window as f64;
    let pct = (f * 100.0).round().min(100.0) as u64;
    let tone = fill_of(w, if f >= FULL { "hn.ChatContextTone.full" } else if f >= WARN { "hn.ChatContextTone.warn" } else { "hn.ChatContextTone.ok" });
    if let Some(p) = v.part("pct") {
        p.text(&format!("{pct}%"));
        set_fill(p, tone);
    }
    let tip = Tooltip { text: format!("{} of {} tokens ({pct}%)", tokens(used), tokens(window)).as_str().into() };
    if v.entity().try_cloned::<&Tooltip>().as_ref() != Some(&tip) {
        v.set(tip);
    }
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

/// The open menu's highlight (the pointer's and the arrow keys', harness_native_ui_view::menu) and what each of its
/// rows picks (None for a head, a separator or a note), as last drawn: what Return picks.
#[derive(Default)]
pub struct PickNav {
    nav: menu::Nav,
    picks: Vec<Option<String>>,
}

/// A row of the picker's menu.
enum Row {
    Head(&'static str),
    Sep,
    Choice { label: String, about: String, on: bool, action: String },
    /// Why nothing can be chosen here, in a short line.
    Note(String),
}

/// The open menu's rows on its plate (prims.flecs ui.Menu's parts): `models` (`page` Model), the efforts of the current
/// model (Effort), or both under heads (All); what cannot be chosen says why instead. The highlight goes where the
/// pointer (on `hovered`) or the keys put it (`nav`).
#[allow(clippy::too_many_arguments)]
fn menu_rows(
    plate: View<'_>, sid: u64, models: &[ModelChoice], kind: &str, cur: Option<&ModelChoice>, page: Pick, name: &str,
    effort: &str, hovered: Option<EntityView<'_>>, nav: &mut PickNav,
) {
    let mut rows: Vec<Row> = Vec::with_capacity(models.len() + 12); // alloc: the open menu's rows, once a frame
    if models.is_empty() {
        let agent = match kind {
            "codex" => "Codex",
            "claude" | "" => "Claude",
            other => other,
        };
        rows.push(Row::Note(format!("Model list arrives when a {agent} chat starts.")));
    } else {
        if page != Pick::Effort {
            if page == Pick::All {
                rows.push(Row::Head("Model"));
            }
            rows.extend(models.iter().map(|c| Row::Choice {
                label: if c.name.is_empty() { c.id.clone() } else { c.name.clone() },
                about: c.about.clone(),
                on: cur.is_some_and(|x| x.id == c.id),
                action: format!("chat:set:{sid}:model:{}", c.id),
            }));
        }
        if page == Pick::All {
            rows.push(Row::Sep);
            rows.push(Row::Head("Effort"));
        }
        if page != Pick::Model {
            match cur {
                Some(c) if !c.efforts.is_empty() => rows.extend(c.efforts.iter().map(|e| Row::Choice {
                    label: effort_word(e),
                    about: String::new(),
                    on: e == effort,
                    action: format!("chat:set:{sid}:effort:{e}"),
                })),
                Some(_) => rows.push(Row::Note(format!("{name} has no effort levels."))),
                None => rows.push(Row::Note(format!("{name} is not in the model list, so its efforts are unknown."))),
            }
        }
    }
    nav.picks.clear();
    nav.picks.extend(rows.iter().map(|r| match r {
        Row::Choice { action, .. } => Some(action.clone()),
        _ => None,
    }));
    // The row the pointer is on (one of the plate's children, or inside one), if it can be chosen.
    let plate_id = plate.id();
    let under = hovered
        .and_then(|h| std::iter::successors(Some(h), |n| n.parent()).take(16).find(|n| n.parent().is_some_and(|p| p.id() == plate_id)))
        .and_then(|row| row.name().strip_prefix('r').and_then(|i| i.parse::<usize>().ok()))
        .filter(|&i| nav.picks.get(i).is_some_and(Option::is_some));
    nav.nav.pointer(under);
    let hot = nav.nav.hot;
    let keyed = rows.iter().enumerate().map(|(i, r)| {
        let prefab = match r {
            Row::Head(_) => "ui.MenuHead",
            Row::Sep => "ui.MenuSep",
            Row::Choice { on: true, .. } => "ui.MenuItemOn",
            Row::Choice { .. } => "ui.MenuItem",
            Row::Note(_) => "ui.MenuNote",
        };
        (format!("r{i}"), prefab, (i, r))
    });
    view::for_each_of(plate, keyed, |v, (i, r), _| match r {
        Row::Head(s) => {
            if let Some(l) = v.part("label") {
                l.text(s);
            }
        }
        Row::Note(s) => {
            if let Some(l) = v.part("label") {
                l.text(s);
            }
        }
        Row::Choice { label, about, action, .. } => {
            menu::light(v.entity(), hot == Some(i));
            v.action(action);
            if let Some(l) = v.part("label") {
                l.text(label);
            }
            if let Some(a) = v.part("about") {
                a.text(about);
            }
        }
        Row::Sep => {}
    });
}

