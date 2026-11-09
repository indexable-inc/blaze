//! The Archive pane (`hn.ArchivePane`): everything the live tree no longer shows (the model's `ArchiveRule` decides
//! what), as a searchable, filterable list by day. It is a query over `Archived` with `Created` / `Ended`, the session
//! a run ran under its parent; older runs come a page at a time from the daemon (`runs` with `before`) into the world
//! (`Paged`) as the list's foot scrolls into view, never the whole history at once.
//!
//! Its state is on the Archive's entity (`model::ARCHIVE`): [`ArchiveFilter`] (the chip that is on) and [`ArchiveQuery`]
//! (the search); a session whose runs show under it has [`ArchiveOpen`]. A row pressed opens its object in the focused
//! pane with `(Back, archive)` on the pane, which cmd+[ or the title bar's chevron follows back.
use super::*;
use crate::pane::Back;
use std::collections::HashSet;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, Vec2};

/// The Archive list's scroll: its engine (AppKit's physics, as the tree's) and the origin it last showed.
pub struct ListScroll {
    engine: Engine,
    clock: std::time::Instant,
    y: f32,
    /// Bumped for each origin set from outside the engine: the engine takes it as a command.
    rev: usize,
}

impl Default for ListScroll {
    fn default() -> Self {
        Self { engine: Engine::new(sidebar::config(0.0, 0.0, 0.0, 0.0, 0, 0.5), Mutation::None), clock: std::time::Instant::now(), y: 0.0, rev: 0 }
    }
}

/// The Archive's note when its filter matches nothing (Kip puzzles over it).
const NOTHING_MATCHES: &str = "Nothing matches";

/// On the Archive's entity: which chip is on (0 All, 1 Sessions, 2 Runs, 3 Failed).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ArchiveFilter {
    pub chip: u8,
}

/// On the Archive's entity: what its search line holds.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct ArchiveQuery {
    pub s: ScriptString,
}

/// On an archived session: its archived runs show under it in the Archive.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ArchiveOpen;

pub fn register(world: &World) {
    world.component::<ArchiveFilter>().writable();
    world.component::<ArchiveQuery>().writable();
    world.component::<ArchiveOpen>();
}

const CHIPS: [&str; 4] = ["All", "Sessions", "Runs", "Failed"];
/// The search icon (cod-search) and the back chevron (cod-chevron-left).
const SEARCH_ICON: &str = "\u{EA6D}";
pub const BACK_ICON: &str = "\u{EAB5}";

/// A row of the Archive's list.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ARow {
    Section(&'static str),
    /// An archived object at `depth`, with `kids` archived runs under it (shown when it is open).
    Item {
        e: u64,
        depth: usize,
        kids: usize,
    },
    More,
}

/// Local midnight today (ms since the epoch).
fn midnight(now: u64) -> u64 {
    let secs = (now / 1000) as libc::time_t;
    // SAFETY: localtime_r writes the broken-down time into `tm`, which lives for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let into = u64::try_from(tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec).unwrap_or(0) * 1000
        + now % 1000;
    now.saturating_sub(into)
}

const DAY: u64 = 86_400_000;

/// The day heading for `ms`.
fn section(ms: u64, today: u64) -> &'static str {
    if ms >= today {
        "Today"
    } else if ms >= today.saturating_sub(DAY) {
        "Yesterday"
    } else if ms >= today.saturating_sub(6 * DAY) {
        "This week"
    } else {
        "Older"
    }
}

/// When, for a row: `14:32` today and yesterday, `Mon 14:32` this week, `Oct 12` before.
fn fmt_when(ms: u64, today: u64) -> String {
    if ms == 0 {
        return String::new();
    }
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: as in `midnight`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let hm = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    match section(ms, today) {
        "Today" => hm,
        "Yesterday" => format!("Yest. {hm}"),
        "This week" => format!("{} {hm}", DAYS[tm.tm_wday.clamp(0, 6) as usize]),
        _ => format!("{} {}", MONTHS[tm.tm_mon.clamp(0, 11) as usize], tm.tm_mday),
    }
}

impl App {
    /// The Archive's entity.
    pub(super) fn archive_view(&self) -> u64 {
        model::archive_view(self.world.world_ref())
    }

    fn archive_chip(&self) -> u8 {
        self.world
            .entity_from_id(self.archive_view())
            .try_cloned::<&ArchiveFilter>()
            .map_or(0, |f| f.chip)
    }

    fn archive_query(&self) -> String {
        self.world
            .entity_from_id(self.archive_view())
            .try_cloned::<&ArchiveQuery>()
            .map(|q| q.s.as_str().to_owned())
            .unwrap_or_default()
    }

    /// Set the search to `q` (the list goes back to its top).
    fn set_archive_query(&mut self, q: &str) {
        let v = self.world.entity_from_id(self.archive_view());
        if v.try_cloned::<&ArchiveQuery>().is_none_or(|o| o.s.as_str() != q) {
            v.set(ArchiveQuery { s: q.into() });
            self.archive_scroll_to(0.0);
        }
        self.poke();
    }

    /// Turn chip `i` on.
    pub(super) fn set_archive_chip(&mut self, i: u8) {
        let v = self.world.entity_from_id(self.archive_view());
        if v.try_cloned::<&ArchiveFilter>().is_none_or(|f| f.chip != i) {
            v.set(ArchiveFilter { chip: i });
            self.archive_scroll_to(0.0);
        }
        self.poke();
    }

    /// Show the Archive in the focused pane.
    pub fn open_archive(&mut self) {
        let v = self.archive_view();
        self.select(Some(v));
    }

    /// Open `e` (from the Archive's list or the palette) in the focused pane, with the way back to the Archive.
    pub(super) fn open_from_archive(&mut self, e: u64) {
        self.select(Some(e));
        if let Some(p) = self.focused_pane() {
            let v = self.archive_view();
            self.world.entity_from_id(p).add((Back::id(), v));
        }
        self.poke();
    }

    /// Where the focused pane's back affordance goes, if anywhere.
    pub(super) fn back_target(&self) -> Option<u64> {
        let p = self.focused_pane()?;
        self.world.entity_from_id(p).target(Back::id(), 0).filter(|t| t.is_alive()).map(|t| *t.id())
    }

    /// Go back (cmd+[, the title bar's chevron): whether there was somewhere to go.
    pub(super) fn go_back(&mut self) -> bool {
        let Some(t) = self.back_target() else { return false };
        self.select(Some(t));
        true
    }

    /// Fold or unfold archived session `e`'s runs.
    pub(super) fn toggle_archive_open(&mut self, e: u64) {
        let o = self.world.entity_from_id(e);
        if o.has(ArchiveOpen::id()) {
            o.remove(ArchiveOpen::id());
        } else {
            o.add(ArchiveOpen::id());
        }
        self.poke();
    }

    /// The pane showing the Archive (the focused one first), if any.
    fn archive_pane(&self) -> Option<u64> {
        let w = self.world.world_ref();
        let shows = |p: u64| {
            model::shows(w, p).is_some_and(|o| model::pane_kind(o) == model::PaneKind::Archive)
        };
        self.focused_pane()
            .filter(|&p| shows(p))
            .or_else(|| self.panes().into_iter().find(|&p| shows(p)))
    }

    /// The Archive's list node (the scroll), when a pane shows it.
    pub(super) fn archive_list(&self) -> Option<u64> {
        View::of(self.world.entity_from_id(self.archive_pane()?)).part("list").map(View::id)
    }

    /// Whether `e` failed: killed, lost, or a nonzero exit.
    fn failed(&self, e: u64) -> bool {
        matches!(self.status_of(e), "killed" | "lost")
            || self
                .world
                .entity_from_id(e)
                .try_cloned::<&model::ExitCode>()
                .is_some_and(|c| c.code != 0)
    }

    fn name_of(&self, e: u64) -> String {
        let n = self
            .world
            .entity_from_id(e)
            .try_cloned::<&model::Name>()
            .map(|n| n.s.as_str().to_owned())
            .unwrap_or_default();
        n.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The Archive's rows for its chip and search: day headings over the archived objects newest first, a session's
    /// archived runs under it while it is open (All and Sessions, no search); flat matches otherwise; the next page's
    /// row at the foot until the daemon has no older runs. With them, the note its empty state says (none while it has
    /// rows, or while older pages may still hold some).
    fn archive_rows(&self) -> (Vec<(String, &'static str, ARow)>, Option<&'static str>) {
        let w = self.world.world_ref();
        let chip = self.archive_chip();
        let q = self.archive_query().to_lowercase();
        let q = q.trim();
        let items: Vec<u64> = model::archived(w).into_iter().rev().collect();
        let set: HashSet<u64> = items.iter().copied().collect();
        let is_session = |e: u64| w.entity_from_id(e).has(model::Object::id());
        let nested = q.is_empty() && chip <= 1;
        let parent_in = |e: u64| {
            w.entity_from_id(e)
                .parent()
                .filter(|p| p.has(model::Object::id()))
                .map(|p| *p.id())
                .filter(|p| set.contains(p))
        };
        let chip_ok = |e: u64| match chip {
            1 => is_session(e),
            2 => !is_session(e),
            3 => self.failed(e),
            _ => true,
        };
        let q_ok = |e: u64| {
            q.is_empty() || {
                let o = w.entity_from_id(e);
                let id = self.run_of(e).map_or_else(
                    || self.session_of(e).map(|s| format!("s{s}")).unwrap_or_default(),
                    |(n, _)| format!("r{n}"),
                );
                let cwd = o
                    .try_cloned::<&model::Cwd>()
                    .map(|c| c.s.as_str().to_lowercase())
                    .unwrap_or_default();
                self.name_of(e).to_lowercase().contains(q) || id == q || cwd.contains(q)
            }
        };
        let mut kids: HashMap<u64, Vec<u64>> = HashMap::new();
        if nested {
            for &e in &items {
                if let Some(p) = parent_in(e) {
                    kids.entry(p).or_default().push(e);
                }
            }
        }
        let today = midnight(model::now_ms());
        let mut out: Vec<(String, &'static str, ARow)> = Vec::new();
        let mut last: Option<&'static str> = None;
        for &e in &items {
            if !chip_ok(e) || !q_ok(e) || (nested && parent_in(e).is_some()) {
                continue;
            }
            let sec = section(model::when(w.entity_from_id(e)), today);
            if last != Some(sec) {
                last = Some(sec);
                out.push((format!("sec:{sec}"), "hn.ArchSection", ARow::Section(sec)));
            }
            let under = kids.get(&e).map_or(&[][..], Vec::as_slice);
            out.push((
                format!("a{e}"),
                "hn.ArchRow",
                ARow::Item { e, depth: 0, kids: under.len() },
            ));
            if w.entity_from_id(e).has(ArchiveOpen::id()) {
                for &k in under {
                    out.push((
                        format!("a{k}"),
                        "hn.ArchRow",
                        ARow::Item { e: k, depth: 1, kids: 0 },
                    ));
                }
            }
        }
        let exhausted = w.try_get::<&model::History>(|h| h.exhausted).unwrap_or(false);
        // "Nothing archived yet" only once every older page is in: before that the foot's "Load older runs" is the truth.
        let note = (out.is_empty() && (exhausted || !items.is_empty()))
            .then_some(if items.is_empty() { "Nothing archived yet" } else { NOTHING_MATCHES });
        if !exhausted && chip != 1 {
            out.push(("more".into(), "hn.ArchMore", ARow::More));
        }
        (out, note)
    }

    /// Fill an Archive pane (the title bar names and counts it, [`Self::archive_subtitle`]): its search line, chips and list; and page in older runs once the list's
    /// foot is in view.
    pub(super) fn fill_archive_pane(&self, pane: u64) {
        let w = self.world.world_ref();
        let pv = View::of(w.entity_from_id(pane));
        let (clear, chosen) = (tone(w, "clear"), tone(w, "selection"));
        let (strong, muted, faint) = (tone(w, "ink_strong"), tone(w, "ink_muted"), tone(w, "ink_faint"));
        let q = self.archive_query();
        if let Some(head) = pv.part("head") {
            if let Some(s) = head.part("search") {
                if let Some(p) = s.part("icon") {
                    p.icon(SEARCH_ICON);
                }
                if let Some(p) = s.part("text") {
                    p.text(if q.is_empty() { "Search archive\u{2026}" } else { q.as_str() });
                    set_if(p, if q.is_empty() { faint } else { strong });
                }
            }
            if let Some(list) = head.part("chips") {
                let on = self.archive_chip();
                for_each(
                    list,
                    "hn.Chip",
                    CHIPS.iter().enumerate().map(|(i, c)| (format!("c{i}"), (i as u8, *c))),
                    |row, (i, label), new| {
                        if new {
                            row.set(HoverAmt::default());
                        }
                        row.action(&format!("arch:chip:{i}"));
                        set_if(row, if i == on { chosen } else { clear });
                        if let Some(p) = row.part("label") {
                            p.text(label);
                            set_if(p, if i == on { strong } else { muted });
                        }
                    },
                );
            }
        }
        let Some(list) = pv.part("list") else { return };
        let Some(lazy) = list.part("rows") else { return };
        let looks = RowLooks::of(w, clear);
        let step = crate::look::width(w, "indent");
        let today = midnight(model::now_ms());
        let loading = w.try_get::<&model::History>(|h| h.loading).unwrap_or(false);
        let mut foot_seen = false;
        let list_rect = list.entity().try_cloned::<&Laid>();
        // Only the rows in view are entities (recycled as the list scrolls); paging asks the daemon once the foot shows.
        let (rows, note) = self.archive_rows();
        // The empty state: centred in the room the rows leave (scene/widgets/archive.flecs `empty`), not a row of them.
        if let Some(empty) = list.part("empty") {
            for_each(empty, "hn.ArchNote", note.map(|n| ("note", n)), |row, n, _| {
                if let Some(p) = row.part("label") {
                    p.text(n);
                }
                // Asleep over an empty Archive, puzzled by a filter that matches nothing.
                if let Some(k) = row.part("kip") {
                    let mood = (n == NOTHING_MATCHES).then_some("thinking");
                    super::panes::show_kip(k, Some(("hn.KipArchive", mood)), 0.0);
                }
            });
        }
        view::lazy_for_each_of(
            lazy,
            rows.len() as u32,
            |i| rows[i as usize].0.as_str(),
            |i| rows[i as usize].1,
            |row, i, fresh| {
                if fresh {
                    row.set(HoverAmt::default());
                }
                match rows[i as usize].2 {
                    ARow::Section(s) => {
                        if let Some(p) = row.part("label") {
                            p.text(s);
                        }
                    }
                    ARow::More => {
                        if let Some(p) = row.part("label") {
                            p.text(if loading {
                                "Loading older runs\u{2026}"
                            } else {
                                "Load older runs"
                            });
                        }
                        let laid = row.entity().try_cloned::<&Laid>();
                        foot_seen = matches!((laid, list_rect), (Some(r), Some(l)) if r.h > 0.0 && r.y < l.y + l.h);
                    }
                    ARow::Item { e, depth, kids } => {
                        self.fill_arch_row(row, e, depth, kids, &looks, step, today)
                    }
                }
            },
        );
        if foot_seen && !loading {
            self.request_older();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_arch_row(
        &self,
        row: View<'_>,
        e: u64,
        depth: usize,
        kids: usize,
        looks: &RowLooks,
        step: f32,
        today: u64,
    ) {
        let w = self.world.world_ref();
        let o = w.entity_from_id(e);
        row.action(&format!("arch:open:{e}"));
        set_if(row, looks.clear);
        if let Some(p) = row.part("indent") {
            let s = Size { w: Sizing::fixed(depth as f32 * step), h: Sizing::fixed(1.0) };
            if p.entity().try_cloned::<&Size>() != Some(s) {
                p.set(s);
            }
        }
        if let Some(d) = row.part("disclose") {
            let de = d.entity();
            let open = o.has(ArchiveOpen::id());
            if kids > 0 {
                if !de.has(harness_native_ui_input_api::Pressable::id()) {
                    de.add(harness_native_ui_input_api::Pressable::id());
                }
                d.action(&format!("arch:fold:{e}"));
                if let Some(i) = d.part("icon") {
                    i.icon(crate::look::CHEVRON);
                    crate::look::disclose(i.entity(), open);
                    crate::look::hover_show(i.entity(), row.id(), true);
                }
            } else {
                if de.has(harness_native_ui_input_api::Pressable::id()) {
                    de.remove(harness_native_ui_input_api::Pressable::id());
                    de.remove(harness_native_ui_input_api::Hoverable::id());
                    de.remove(harness_native_ui_input_api::Focusable::id());
                }
                if let Some(i) = d.part("icon") {
                    i.icon("");
                }
            }
        }
        let run = self.run_of(e);
        if run.is_none() {
            if let Some(p) = row.part("glyph") {
                crate::look::kind_mark(w, p.entity(), model::kind_name(o));
            }
        } else if let Some(p) = row.part("glyph") {
            crate::look::show_glyph(p.entity(), crate::look::RUN_ICON);
            set_if(p, self.dot_tone(e));
        }
        if let Some(p) = row.part("name") {
            let name = self.name_of(e);
            p.text(&match run {
                Some((n, _)) => format!("r{n}  {name}"),
                None => name,
            });
        }
        if let Some(p) = row.part("under") {
            // A run shown on its own: the session it ran under. A session: how many runs it holds.
            let text = match (run, depth) {
                (Some(_), 0) => o
                    .parent()
                    .filter(|p| p.has(model::Object::id()))
                    .map(|p| format!("in {}", self.name_of(*p.id())))
                    .unwrap_or_default(),
                (None, _) if kids > 0 => format!("{kids} run{}", if kids == 1 { "" } else { "s" }),
                _ => String::new(),
            };
            p.text(&text);
        }
        if let Some(b) = row.part("status") {
            let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
            let (text, ink, wash) = match (self.status_of(e), code) {
                // Closed (cmd+W) but its process is alive in the daemon: say so, not a bare "running".
                ("running", _) if o.has(model::Object::id()) => {
                    ("still running".to_owned(), looks.accent, looks.clear)
                }
                ("running", _) => ("running".to_owned(), looks.accent, looks.clear),
                ("killed", _) => ("killed".to_owned(), looks.failed, looks.failed_wash),
                ("lost", _) => ("lost".to_owned(), looks.lost, looks.clear),
                (_, Some(c)) if c != 0 => (format!("exit {c}"), looks.failed, looks.failed_wash),
                ("pending" | "skipped", _) => {
                    (self.status_of(e).to_owned(), looks.faint, looks.clear)
                }
                _ => ("done".to_owned(), looks.faint, looks.clear),
            };
            if let Some(t) = b.part("code") {
                t.text(&text);
                set_if(t, ink);
            }
            set_if(b, wash);
            if b.entity().try_cloned::<&harness_native_ui_layout_api::Padding>()
                != Some(looks.badge_on)
            {
                b.set(looks.badge_on);
            }
        }
        if let Some(t) = row.part("took").and_then(|c| c.part("text")) {
            t.text(&run.and_then(|_| self.run_took(e)).unwrap_or_default());
        }
        if let Some(t) = row.part("time").and_then(|c| c.part("text")) {
            t.text(&fmt_when(model::when(o), today));
        }
    }

    /// The title bar's line under "Archive": how many objects it holds, and while a chip or a search narrows it, how
    /// many of them show.
    pub(super) fn archive_subtitle(&self) -> String {
        let n = model::archived(self.world.world_ref()).len();
        let all = format!("{n} archived");
        let chip = self.archive_chip();
        if chip == 0 && self.archive_query().trim().is_empty() {
            return all;
        }
        let shown =
            self.archive_rows().0.iter().filter(|(_, _, r)| matches!(r, ARow::Item { .. })).count();
        let what =
            if chip == 0 { "matching".to_owned() } else { CHIPS[usize::from(chip)].to_lowercase() };
        format!("{all}  \u{00B7}  {shown} {what}")
    }

    /// Ask the daemon for the page of runs older than the oldest the world holds.
    pub(super) fn request_older(&self) {
        let w = self.world.world_ref();
        if w.try_get::<&model::History>(|h| h.loading || h.exhausted).unwrap_or(true) {
            return;
        }
        let mut oldest: Option<u64> = None;
        self.world
            .new_query::<&model::Run>()
            .each_entity(|_, r| oldest = Some(oldest.map_or(r.n, |o| o.min(r.n))));
        match oldest {
            Some(n) if n > 1 => {
                w.set(model::History { loading: true, exhausted: false });
                self.link.runs_page(n);
            }
            // Nothing older than r1 (or no runs at all yet: the poll brings the newest first).
            Some(_) => w.set(model::History { loading: false, exhausted: true }),
            None => {}
        }
    }

    fn archive_scroll_to(&self, y: f32) {
        if let Some(l) = self.archive_list() {
            let e = self.world.entity_from_id(l);
            if e.try_cloned::<&ScrollStateOf>().is_none_or(|s| (s.y - y).abs() > 0.001) {
                e.set(ScrollStateOf { x: 0.0, y });
            }
        }
    }

    /// A scroll over the Archive's list: its physics take it (AppKit's, as the tree's: the fingers' drag, the momentum
    /// after, the rubber band at either end). Whether it was over it.
    pub(super) fn archive_wheel(&mut self, ev: &sidebar::Wheel, z: f32) -> bool {
        let Some(l) = self.archive_list() else { return false };
        let Some(r) = self.world.entity_from_id(l).try_cloned::<&Laid>() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        if !(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h) {
            return false;
        }
        let t = self.arch_scroll.clock.elapsed().as_secs_f64();
        let k = if ev.precise { 1.0 / z } else { 1.0 };
        self.arch_scroll.engine.wheel(
            Input {
                delta: Vec2::new(f64::from(ev.dx * k), f64::from(ev.dy * k)),
                precise: ev.precise,
                direct: Phase::from_ns(ev.phase),
                momentum: Phase::from_ns(ev.momentum),
                timestamp: t,
            },
            t,
        );
        self.poke();
        true
    }

    /// Step the Archive list's scroll one frame: measure it from the last layout, advance the physics and show its
    /// origin. An origin something else set (a key that keeps the selection in view, a search that starts over, a drag on
    /// the overlay scroller) is the origin from then.
    pub(super) fn step_archive_scroll(&mut self) {
        let Some(l) = self.archive_list() else { return };
        let le = self.world.entity_from_id(l);
        let Some(r) = le.try_cloned::<&Laid>().filter(|r| r.h > 0.0) else { return };
        let content = le.try_cloned::<&harness_native_ui_layout_api::ScrollExtent>().map_or(r.h, |x| x.h);
        let max = (content - r.h).max(0.0);
        let shown = le.try_cloned::<&ScrollStateOf>().map_or(0.0, |s| s.y);
        if (shown - self.arch_scroll.y).abs() > 0.01 {
            self.arch_scroll.y = shown;
            self.arch_scroll.rev += 1;
        } else if self.arch_scroll.y > max && !self.arch_scroll.engine.needs_frame() {
            // The list got shorter under the origin: back inside it.
            self.arch_scroll.y = max;
            self.arch_scroll.rev += 1;
        }
        let t = self.arch_scroll.clock.elapsed().as_secs_f64();
        self.arch_scroll.engine.configure(sidebar::config(r.w, r.h, content, self.arch_scroll.y, self.arch_scroll.rev, 1.0 / f64::from(self.grid.scale.max(0.5))));
        self.arch_scroll.engine.advance(t);
        let y = self.arch_scroll.engine.report().y as f32;
        self.arch_scroll.y = y;
        self.archive_scroll_to(y);
        if self.arch_scroll.engine.needs_frame() {
            self.poke();
        }
    }

    /// Whether the focused pane shows the Archive (typing goes to its search).
    pub(super) fn archive_focused(&self) -> bool {
        self.focused_pane()
            .and_then(|p| model::shows(self.world.world_ref(), p))
            .is_some_and(|o| model::pane_kind(o) == model::PaneKind::Archive)
    }

    /// The Archive's search, edited by its keys (scene/20-keys.flecs, ctx.Archive): cleared (Escape), its last character
    /// erased (Backspace), or `typed` added (a key no binding takes).
    pub(super) fn archive_search(&mut self, edit: SearchEdit<'_>) {
        let mut q = self.archive_query();
        match edit {
            SearchEdit::Clear => q.clear(),
            SearchEdit::Erase => {
                q.pop();
            }
            SearchEdit::Type(text) => {
                let typed: String = text
                    .chars()
                    .filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))
                    .collect();
                if typed.is_empty() {
                    return;
                }
                q.push_str(&typed);
            }
        }
        self.set_archive_query(&q);
    }

    /// Return in the Archive: open its first row.
    pub(super) fn archive_open_first(&mut self) {
        let first = self.archive_rows().0.into_iter().find_map(|(_, _, r)| match r {
            ARow::Item { e, .. } => Some(e),
            _ => None,
        });
        if let Some(e) = first {
            self.open_from_archive(e);
        }
    }

    /// Text committed (an input method, a paste) while the Archive has the keyboard: into its search. Whether it took it.
    pub(super) fn archive_text(&mut self, text: &str) -> bool {
        if !self.archive_focused() {
            return false;
        }
        let q = self.archive_query() + text.lines().next().unwrap_or("");
        self.set_archive_query(&q);
        true
    }
}

use harness_native_ui_layout_api::ScrollState as ScrollStateOf;

/// An edit of the Archive's search.
pub(super) enum SearchEdit<'a> {
    Clear,
    Erase,
    Type(&'a str),
}
